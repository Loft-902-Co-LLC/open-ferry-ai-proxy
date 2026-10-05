//! Seeded random input for the translators from Gemini `generateContent`
//! clients to Codex, Claude and Chat Completions.
//!
//! Requests have contents in every role, with text, thought, inline and file
//! data, function call and function response parts, in their camelCase and
//! snake_case spellings; function declarations whose names are long, collide
//! once cut or hold multi-byte characters, with `parameters` or
//! `parametersJsonSchema` schemas whose types are in any case; tool configs;
//! generation configs with thinking levels and budgets; service tiers and
//! client user IDs. Each asks for a stream or not. They leave out what we
//! write differently on purpose, as the translators' docs say: a
//! `temperature` or `topP` that isn't a finite number, a schema `type` that
//! isn't a string or a property named `type`, and property names holding
//! sjson path syntax.
//!
//! Responses reuse other generators' streams: Codex's ([`super::response`])
//! answering a Gemini request that declares the tools they call, sometimes
//! with a `created_at`; Claude's ([`super::claude_chat`]); and Chat
//! Completions' ([`super::openai_chat`]), some with arguments only a lenient
//! reader can make sense of. Upstream copies some tool arguments as they are:
//! a Codex call's that start with `{` but don't parse, and a Claude call's
//! input that doesn't parse. Its output is then not JSON, and ours holds `{}`
//! (see the translators' docs), so those arguments are replaced first.

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};

use serde_json::{Map, Value, json};

use super::openai_chat::{self, ARGUMENTS, Call};
use super::{BUDGETS, Rng, SERVICE_TIERS, num, to_object};
use crate::cases::Case;

/// Builds `count` random Gemini requests, each asking for a stream or not.
pub fn request_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed, index);
            let model = generator.rng.pick(MODELS);
            let request = generator.request();
            let text = generator.render(&request);
            let stream = generator.rng.chance(50);
            Case::new(format!("random-{seed}-{index}"), model, text)
                .with_options(json!({ "stream": stream }))
        })
        .collect()
}

/// Builds `count` random Codex streams answering random Gemini requests, and
/// a non-streaming case from each one's final event.
pub fn codex_event_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    let mixed = seed.rotate_left(27);
    let (streams, finals) = super::response::cases(mixed, count);
    streams
        .into_iter()
        .zip(finals)
        .enumerate()
        .map(|(index, (stream, last))| {
            let mut generator = Generator::new(mixed, index as u64);
            let model = generator.rng.pick(&["gemini-2.5-pro", "gpt-5", "", " "]);
            let claude: Value = serde_json::from_str(&stream.request).unwrap_or_default();
            let request = generator.codex_original(&claude);
            let request = generator.render(&request);
            let created_at = generator
                .rng
                .chance(40)
                .then(|| num(generator.rng.pick(CREATED_AT)));
            let lines = stream
                .events
                .iter()
                .map(|line| codex_line(line, created_at.as_ref()))
                .collect();
            let body = codex_event(&last.events[0], created_at.as_ref())
                .map_or_else(|| last.events[0].clone(), |event| event.to_string());
            let case = |events| Case {
                events,
                ..Case::new(format!("random-{seed}-{index}"), model, request.clone())
            };
            (case(lines), case(vec![body]))
        })
        .unzip()
}

/// Builds `count` random Claude event streams, and a non-streaming case from
/// the whole of each.
pub fn claude_event_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    let (streams, finals) = super::claude_chat::event_cases(seed.rotate_left(13), count);
    streams
        .into_iter()
        .zip(finals)
        .enumerate()
        .map(|(index, (stream, last))| {
            let name = format!("random-{seed}-{index}");
            let (mut stream, mut last) = repair_claude_input(stream, last);
            stream.name.clone_from(&name);
            last.name = name;
            (stream, last)
        })
        .unzip()
}

/// Builds `count` random Chat Completions streams, and a non-streaming case
/// from a whole response for each.
pub fn openai_event_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    let calls: [Call; 3] = [
        ("get_weather".to_owned(), ARGUMENTS),
        ("search".to_owned(), LENIENT_ARGUMENTS),
        ("outil_météo".to_owned(), LENIENT_ARGUMENTS),
    ];
    (0..count as u64)
        .map(|index| {
            let mut generator = openai_chat::Generator::new(seed.rotate_left(19), index);
            let lines = generator.stream(&calls);
            let body = generator.body(&calls);
            let case = |events| Case::response(format!("random-{seed}-{index}"), "", events);
            (case(lines), case(vec![body]))
        })
        .unzip()
}

/// A Codex stream case with the arguments upstream would copy as broken JSON
/// replaced.
pub fn repair_codex_case(mut case: Case) -> Case {
    case.events = case
        .events
        .iter()
        .map(|line| codex_line(line, None))
        .collect();
    case
}

/// A Codex non-streaming case with the arguments upstream would copy as
/// broken JSON replaced.
pub fn repair_codex_final(mut case: Case) -> Case {
    for event in &mut case.events {
        if let Some(repaired) = codex_event(event, None) {
            *event = repaired.to_string();
        }
    }
    case
}

/// A Claude stream case and the non-streaming case for the same events, with
/// every tool input emptied where upstream would write one that isn't JSON
/// in either.
pub fn repair_claude_input(mut stream: Case, mut last: Case) -> (Case, Case) {
    let body_lines = |case: &Case| -> Vec<String> {
        case.events
            .first()
            .map(|body| body.split('\n').map(str::to_owned).collect())
            .unwrap_or_default()
    };
    if writes_invalid_input(&stream.events) || writes_invalid_input(&body_lines(&last)) {
        stream.events = stream.events.iter().map(|line| empty_input(line)).collect();
        if let Some(body) = last.events.first_mut() {
            *body = body
                .split('\n')
                .map(empty_input)
                .collect::<Vec<_>>()
                .join("\n");
        }
    }
    (stream, last)
}

/// Models with effort levels, with budgets only, unknown to the catalog, and
/// names it has to clean up.
const MODELS: &[&str] = &[
    "claude-opus-4-6",
    "claude-opus-4-6",
    "claude-opus-4-7",
    "claude-sonnet-4-6",
    "claude-sonnet-4-5-20250929",
    "claude-sonnet-4-5-20250929",
    "claude-haiku-4-5-20251001",
    "claude-3-5-haiku-20241022",
    "claude-opus-4-6(high)",
    " claude-opus-4-6 ",
    "gpt-5",
    "gpt-5-codex",
    "gemini-2.5-pro",
    "",
];

/// Schema types as Gemini writes them, in upper case, and in other cases,
/// including ones Go and Rust might lowercase differently.
const TYPES: &[&str] = &[
    "OBJECT",
    "OBJECT",
    "object",
    "Object",
    "STRING",
    "string",
    "INTEGER",
    "NUMBER",
    "BOOLEAN",
    "ARRAY",
    "array",
    "NULL",
    "TYPE_UNSPECIFIED",
    "",
    "ΑΣ",
    "İNTEGER",
];

/// Property names; none is sjson path syntax or `type`.
const PROPERTY_NAMES: &[&str] = &[
    "city", "unit", "count", "名前", "a b", "$id", "pattern", "items", "x-y", "2",
];

/// Thinking levels, including ones the translators map, ones they don't
/// know, and ones Go and Rust might lowercase differently.
const LEVELS: &[&str] = &[
    "low", "medium", "high", "HIGH", " Medium ", "minimal", "none", "auto", "xhigh", "max", "",
    "unknown", "MAXİMUM",
];

/// Sampling values that are finite however they're read.
const FINITE: &[&str] = &[
    "0",
    "1",
    "0.5",
    "1.50",
    "2.0",
    "0.95",
    "1e-1",
    "1E+0",
    "-0.5",
    "42",
    "-0",
    "1e30",
    // Halfway between two shortest decimals, which Go rounds to even.
    "2156163594508435.25",
    "2.98023223876953125e-8",
];

/// Token limits, none out of int64's range: Go converts those by the CPU's
/// rules.
const TOKEN_LIMITS: &[&str] = &[
    "1024",
    "0",
    "-1",
    "8192.9",
    "1e3",
    "65536",
    "\"2048\"",
    "null",
    "9223372036854775807",
];

/// MIME types of inline and file data: images, audio, video, documents and
/// none.
const MIME_TYPES: &[&str] = &[
    "image/png",
    "IMAGE/JPEG",
    "image/webp",
    "audio/wav",
    "audio/mpeg",
    "audio/mp3",
    "audio/x-flac",
    "audio/ogg",
    "audio/L16;rate=24000",
    "video/mp4",
    "application/pdf",
    "text/plain",
    "text/csv",
    "application/json",
    "application/octet-stream",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "",
];

const FILE_URIS: &[&str] = &[
    "gs://bucket/report.pdf",
    "https://example.com/cat.png",
    "https://generativelanguage.googleapis.com/v1beta/files/abc123",
    "files/abc123",
    "",
    " ",
];

/// Unix times for `response.created_at`, loosely typed, none far from now.
const CREATED_AT: &[&str] = &[
    "1755225123",
    "1700000000",
    "0",
    "-1",
    "\"1755225123\"",
    "1.7e9",
    "1755225123.75",
    "true",
    "null",
];

/// Arguments that only a lenient reader makes sense of: keys without
/// quotes, text around the object, missing commas, single quotes, numbers
/// in Go's syntax. None holds a number Go reads as infinite or NaN.
const LENIENT_ARGUMENTS: &[&str] = &[
    r#"{"city": "Paris", unit: celsius}"#,
    r#"Sure: {"a": 1, "b": [1, 2], "c": {"d": true}} done"#,
    r#"{"a": 'single', "b": "double"}"#,
    r#"{"a": tru, "n": 1.50, "m": -0}"#,
    r#"{"a.b": 1, "a b": 2, ":lead": 3}"#,
    r#"{"a": "x" "b": "y"}"#,
    r#"{"big": 123456789012345678901234567890, "e": 1e3, "neg": -7}"#,
    r#"{"h": 0x1p3, "u": 1_000, "f": 1_0.5, "bad": 1__0, "x": 0x10, "t": 1e-400}"#,
    r#"{"esc": "line\nnext \"quoted\" \\ back", "s": "/"}"#,
    r#"{"nested": {"inner": [1, {"x": null}]}, "after": false"#,
    r#"{"city":"Paris"}"#,
    "{}",
    "",
];

/// The Gemini request generator. It builds on the Claude request generator
/// for its leaf values: text, numbers and tool names.
struct Generator {
    base: super::Generator,
    /// IDs of the function calls so far, for function responses to answer.
    call_ids: Vec<Value>,
}

impl Deref for Generator {
    type Target = super::Generator;

    fn deref(&self) -> &super::Generator {
        &self.base
    }
}

impl DerefMut for Generator {
    fn deref_mut(&mut self) -> &mut super::Generator {
        &mut self.base
    }
}

impl Generator {
    fn new(seed: u64, index: u64) -> Self {
        Self {
            base: super::Generator {
                // A different mix from the other generators', so cases don't
                // share their random choices.
                rng: Rng(seed.rotate_left(23) ^ index.wrapping_mul(0xC2B2_AE3D_27D4_EB4F)),
                tool_names: Vec::new(),
                tool_use_ids: Vec::new(),
            },
            call_ids: Vec::new(),
        }
    }

    // --- Requests ---

    fn request(&mut self) -> Value {
        let mut fields = Vec::new();
        // Tools come first so the contents can call them.
        if self.rng.chance(50) {
            let tools = self.tools();
            fields.push(("tools", tools));
        }
        let contents = if self.rng.chance(94) {
            self.contents()
        } else {
            self.one_of(&[json!({}), json!("hi"), Value::Null, json!([])])
        };
        fields.push(("contents", contents));
        if self.rng.chance(40) {
            let key = self.rng.pick(&["systemInstruction", "system_instruction"]);
            let system = self.system_instruction();
            fields.push((key, system));
        }
        if self.rng.chance(20) {
            let (key, config) = self.tool_config();
            fields.push((key, config));
        }
        if self.rng.chance(65) {
            let config = if self.rng.chance(96) {
                self.generation_config()
            } else {
                self.one_of(&[json!("config"), Value::Null, json!([])])
            };
            fields.push(("generationConfig", config));
        }
        if self.rng.chance(15) {
            let tier = self.loose_choice(SERVICE_TIERS);
            fields.push(("service_tier", tier));
        }
        if self.rng.chance(12) {
            let metadata = self.one_of(&[
                json!({ "user_id": "user-123" }),
                json!({ "user_id": " " }),
                json!({ "user_id": 42 }),
                json!({ "user_id": "" }),
                json!({}),
                json!("metadata"),
            ]);
            fields.push(("metadata", metadata));
        }
        if self.rng.chance(8) {
            let user = self.one_of(&[json!("client-user"), json!(""), json!(7), json!(" padded ")]);
            fields.push(("user", user));
        }
        if self.rng.chance(5) {
            fields.push(("model", json!("gemini-2.5-pro")));
        }
        self.object(fields)
    }

    fn tools(&mut self) -> Value {
        if self.rng.chance(4) {
            return self.one_of(&[json!({}), json!("tools"), Value::Null]);
        }
        let count = 1 + self.rng.below(3);
        let tools = (0..count)
            .map(|_| match self.rng.below(14) {
                0 => json!({ "googleSearch": {} }),
                1 => json!({ "codeExecution": {} }),
                2 => json!({ "functionDeclarations": "not a list" }),
                3 => json!({ "function_declarations": self.declarations() }),
                _ => json!({ "functionDeclarations": self.declarations() }),
            })
            .collect();
        Value::Array(tools)
    }

    fn declarations(&mut self) -> Value {
        let count = self.rng.below(4);
        Value::Array((0..count).map(|_| self.declaration()).collect())
    }

    fn declaration(&mut self) -> Value {
        let mut fields = Vec::new();
        if self.rng.chance(95) {
            let name = self.tool_name();
            self.tool_names.push(name.clone());
            fields.push(("name", name));
        }
        if self.rng.chance(60) {
            fields.push(("description", self.loose_text()));
        }
        match self.rng.below(10) {
            0..=5 => fields.push(("parameters", self.schema(0))),
            6..=7 => fields.push(("parametersJsonSchema", self.schema(0))),
            8 => {
                fields.push(("parameters", self.schema(0)));
                fields.push(("parametersJsonSchema", self.schema(0)));
            }
            _ => {}
        }
        if self.rng.chance(5) {
            fields.push(("behavior", json!("NON_BLOCKING")));
        }
        self.object(fields)
    }

    fn schema(&mut self, depth: usize) -> Value {
        let nested = depth < 3;
        let mut fields = Vec::new();
        if self.rng.chance(90) {
            fields.push(("type", json!(self.rng.pick(TYPES))));
        }
        if self.rng.chance(15) {
            fields.push((
                "$schema",
                json!("https://json-schema.org/draft/2020-12/schema"),
            ));
        }
        if self.rng.chance(30) {
            fields.push(("description", self.text().into()));
        }
        if nested && self.rng.chance(70) {
            let mut names = PROPERTY_NAMES.to_vec();
            self.rng.shuffle(&mut names);
            names.truncate(self.rng.below(4));
            let properties: Map<String, Value> = names
                .iter()
                .map(|name| ((*name).to_owned(), self.schema(depth + 1)))
                .collect();
            fields.push(("properties", Value::Object(properties)));
            if !names.is_empty() && self.rng.chance(50) {
                let mut required: Vec<Value> = names
                    .iter()
                    .filter(|_| self.rng.chance(60))
                    .map(|name| json!(name))
                    .collect();
                if self.rng.chance(10) {
                    required.push(json!("missing"));
                }
                fields.push(("required", Value::Array(required)));
            }
        }
        if nested && self.rng.chance(20) {
            fields.push(("items", self.schema(depth + 1)));
        }
        if nested && self.rng.chance(8) {
            fields.push((
                "anyOf",
                json!([self.schema(depth + 1), self.schema(depth + 1)]),
            ));
        }
        if self.rng.chance(20) {
            let additional = self.one_of(&[
                json!(true),
                json!(false),
                json!({}),
                json!({ "type": "STRING" }),
                Value::Null,
                json!("false"),
            ]);
            fields.push(("additionalProperties", additional));
        }
        if self.rng.chance(10) {
            fields.push(("enum", json!(["a", "B", 1, "日本"])));
        }
        if self.rng.chance(10) {
            fields.push(("nullable", json!(true)));
        }
        if self.rng.chance(8) {
            fields.push(("format", self.loose_choice(&["date-time", "enum", "int32"])));
        }
        if self.rng.chance(8) {
            fields.push(("minimum", num(self.rng.pick(FINITE_SCHEMA))));
        }
        if self.rng.chance(5) {
            fields.push(("default", self.loose_text()));
        }
        self.object(fields)
    }

    fn contents(&mut self) -> Value {
        let count = self.rng.below(7);
        Value::Array((0..count).map(|_| self.content()).collect())
    }

    fn content(&mut self) -> Value {
        let role = match self.rng.below(100) {
            0..=44 => Some(json!("user")),
            45..=84 => Some(json!("model")),
            85..=88 => Some(json!("function")),
            89..=90 => None,
            _ => Some(self.one_of(&[
                json!("system"),
                json!("tool"),
                json!(""),
                json!("MODEL"),
                json!(5),
                Value::Null,
            ])),
        };
        let model = role.as_ref().is_some_and(|role| role == "model");
        let mut fields = Vec::new();
        if let Some(role) = role {
            fields.push(("role", role));
        }
        if self.rng.chance(97) {
            let parts = if self.rng.chance(95) {
                let count = self.rng.below(5);
                Value::Array((0..count).map(|_| self.part(model)).collect())
            } else {
                self.one_of(&[json!({ "text": "not a list" }), json!("text"), Value::Null])
            };
            fields.push(("parts", parts));
        }
        self.object(fields)
    }

    /// A part of a model turn when `model`, else of a user turn: usually what
    /// that side would send, sometimes what the other would.
    fn part(&mut self, model: bool) -> Value {
        match self.rng.below(100) {
            0..=34 => self.text_part(),
            35..=44 => self.thought_part(),
            45..=52 => self.inline_data_part(),
            53..=58 => self.file_data_part(),
            59..=80 if model => self.function_call_part(),
            59..=80 => self.function_response_part(),
            81..=88 if model => self.function_response_part(),
            81..=88 => self.function_call_part(),
            _ => self.one_of(&[
                json!({}),
                json!("text"),
                Value::Null,
                json!({ "executableCode": { "language": "PYTHON", "code": "print(1)" } }),
                json!({ "codeExecutionResult": { "outcome": "OUTCOME_OK", "output": "1" } }),
                json!({ "text": "both", "functionCall": { "name": "get_weather", "args": {} } }),
                json!({ "videoMetadata": { "startOffset": "1s" } }),
            ]),
        }
    }

    fn text_part(&mut self) -> Value {
        let mut fields = vec![("text", self.loose_text())];
        if self.rng.chance(10) {
            fields.push(("thoughtSignature", json!("CiQBVKhc7tYz")));
        }
        if self.rng.chance(5) {
            fields.push((
                "thought",
                self.one_of(&[json!(false), Value::Null, json!(0)]),
            ));
        }
        self.object(fields)
    }

    fn thought_part(&mut self) -> Value {
        let thought = if self.rng.chance(75) {
            json!(true)
        } else {
            self.bool_like()
        };
        let mut fields = vec![("text", self.text().into()), ("thought", thought)];
        if self.rng.chance(30) {
            fields.push(("thoughtSignature", json!("CiQBVKhc7tYz")));
        }
        self.object(fields)
    }

    fn inline_data_part(&mut self) -> Value {
        let (key, mime_key) = if self.rng.chance(75) {
            ("inlineData", "mimeType")
        } else {
            ("inline_data", "mime_type")
        };
        let mut data = Vec::new();
        if self.rng.chance(95) {
            data.push((mime_key, self.loose_choice(MIME_TYPES)));
        }
        if self.rng.chance(95) {
            let bytes = self.one_of(&[
                json!("aGVsbG8="),
                json!("iVBORw0KGgo="),
                json!(""),
                json!("not base64!"),
                json!(123),
            ]);
            data.push(("data", bytes));
        }
        to_object(vec![(key, self.object(data))])
    }

    fn file_data_part(&mut self) -> Value {
        let (key, uri_key, mime_key) = match self.rng.below(4) {
            0 => ("file_data", "file_uri", "mime_type"),
            1 => ("fileData", "file_uri", "mimeType"),
            _ => ("fileData", "fileUri", "mimeType"),
        };
        let mut data = Vec::new();
        if self.rng.chance(95) {
            data.push((uri_key, self.loose_choice(FILE_URIS)));
        }
        if self.rng.chance(70) {
            data.push((mime_key, self.loose_choice(MIME_TYPES)));
        }
        to_object(vec![(key, self.object(data))])
    }

    fn function_call_part(&mut self) -> Value {
        let mut call = Vec::new();
        if self.rng.chance(95) {
            let name = self.call_name();
            call.push(("name", name));
        }
        if self.rng.chance(90) {
            let args = self.args();
            call.push(("args", args));
        }
        if let Some((key, id)) = self.call_id() {
            self.call_ids.push(id.clone());
            call.push((key, id));
        }
        let mut fields = vec![("functionCall", self.object(call))];
        if self.rng.chance(15) {
            fields.push(("thoughtSignature", json!("CiQBVKhc7tYz")));
        }
        self.object(fields)
    }

    fn function_response_part(&mut self) -> Value {
        let mut response = Vec::new();
        if self.rng.chance(90) {
            let name = self.call_name();
            response.push(("name", name));
        }
        if self.rng.chance(95) {
            let body = self.response();
            response.push(("response", body));
        }
        if !self.call_ids.is_empty() && self.rng.chance(40) {
            let id = self.base.rng.pick(&self.call_ids);
            let key = self.rng.pick(&["id", "id", "call_id"]);
            response.push((key, id));
        } else if let Some((key, id)) = self.call_id() {
            response.push((key, id));
        }
        to_object(vec![("functionResponse", self.object(response))])
    }

    /// A declared tool's name, or another.
    fn call_name(&mut self) -> Value {
        if !self.tool_names.is_empty() && self.rng.chance(60) {
            return self.base.rng.pick(&self.base.tool_names);
        }
        self.tool_name()
    }

    /// An ID for a call or response, under `id` or `call_id`, or none.
    fn call_id(&mut self) -> Option<(&'static str, Value)> {
        let id = match self.rng.below(100) {
            0..=54 => return None,
            55..=79 => json!(format!("call_{}", self.alphanumeric(8))),
            80..=84 => json!(" call_padded "),
            85..=89 => json!(""),
            90..=93 => json!(" "),
            94..=96 => json!(7),
            _ => json!(format!("toolu_{}", self.alphanumeric(12))),
        };
        let key = if self.rng.chance(80) { "id" } else { "call_id" };
        Some((key, id))
    }

    fn args(&mut self) -> Value {
        match self.rng.below(12) {
            0..=3 => json!({ "city": self.text(), "unit": "celsius" }),
            4 => json!({}),
            5 => json!({ "a": [1, num("2.50"), { "b": null }], "c": "<&>" }),
            6 => json!({ "q": "café / x", "n": num("1e3") }),
            7 => json!({ "nested": { "deep": { "deeper": [true, false] } } }),
            8 => json!("{\"city\":\"Paris\"}"),
            9 => self.one_of(&[Value::Null, json!(5), json!([1, 2]), json!(true)]),
            10 => json!({
                "big": num("123456789012345678901234567890"),
                "neg": num("-0"),
                "e": num("1E+2"),
            }),
            _ => json!({ "text": self.loose_text() }),
        }
    }

    fn response(&mut self) -> Value {
        match self.rng.below(10) {
            0..=3 => json!({ "result": self.loose_text() }),
            4 => json!({ "result": { "temp": 21, "unit": "C", "ok": true } }),
            5 => json!({ "output": "done", "extra": [1, num("2.50")] }),
            6 => json!({}),
            7 => self.one_of(&[json!("plain text"), Value::Null, json!(5), json!([1, "a"])]),
            8 => json!({ "result": null }),
            _ => json!({ "content": [{ "text": "x" }] }),
        }
    }

    fn system_instruction(&mut self) -> Value {
        match self.rng.below(10) {
            0..=6 => {
                let count = self.rng.below(4);
                let parts: Vec<Value> = (0..count)
                    .map(|_| {
                        if self.rng.chance(15) {
                            self.thought_part()
                        } else {
                            self.text_part()
                        }
                    })
                    .collect();
                let mut fields = vec![("parts", Value::Array(parts))];
                if self.rng.chance(30) {
                    fields.push(("role", self.loose_choice(&["system", "user", ""])));
                }
                self.object(fields)
            }
            7 => json!({ "parts": { "text": "not a list" } }),
            8 => json!("be brief"),
            _ => {
                let image = self.inline_data_part();
                json!({ "parts": [{ "text": self.loose_text() }, image] })
            }
        }
    }

    fn tool_config(&mut self) -> (&'static str, Value) {
        let mode = self.loose_choice(&["ANY", "ANY", "AUTO", "NONE", "VALIDATED", "any", ""]);
        let names = match self.rng.below(6) {
            0 => None,
            1 | 2 => Some(json!([self.call_name()])),
            3 => Some(json!([self.call_name(), self.call_name()])),
            4 => Some(json!([])),
            _ => Some(self.one_of(&[json!("get_weather"), Value::Null])),
        };
        let (key, config_key, names_key) = if self.rng.chance(80) {
            (
                "toolConfig",
                "functionCallingConfig",
                "allowedFunctionNames",
            )
        } else {
            (
                "tool_config",
                "function_calling_config",
                "allowed_function_names",
            )
        };
        let mut config = vec![("mode", mode)];
        if let Some(names) = names {
            config.push((names_key, names));
        }
        let config = self.object(config);
        (key, to_object(vec![(config_key, config)]))
    }

    fn generation_config(&mut self) -> Value {
        let mut fields = Vec::new();
        if self.rng.chance(40) {
            fields.push(("temperature", self.sampling()));
        }
        if self.rng.chance(40) {
            fields.push(("topP", self.sampling()));
        }
        if self.rng.chance(25) {
            fields.push(("topK", num(self.rng.pick(TOKEN_LIMITS))));
        }
        if self.rng.chance(40) {
            fields.push(("maxOutputTokens", num(self.rng.pick(TOKEN_LIMITS))));
        }
        if self.rng.chance(25) {
            let stops = match self.rng.below(6) {
                0..=2 => {
                    let count = self.rng.below(4);
                    Value::Array((0..count).map(|_| self.loose_text()).collect())
                }
                3 => json!("END"),
                4 => json!([]),
                _ => json!(["END", null, 5]),
            };
            fields.push(("stopSequences", stops));
        }
        if self.rng.chance(10) {
            let count = self.one_of(&[json!(1), json!(2), json!("3"), json!(0), num("1.5")]);
            fields.push(("candidateCount", count));
        }
        if self.rng.chance(10) {
            let modalities = self.one_of(&[
                json!(["TEXT"]),
                json!(["TEXT", "IMAGE"]),
                json!(["AUDIO"]),
                json!(["text", "image"]),
                json!("TEXT"),
                json!([]),
            ]);
            fields.push(("responseModalities", modalities));
        }
        if self.rng.chance(50) {
            fields.push(("thinkingConfig", self.thinking_config()));
        }
        if self.rng.chance(10) {
            let key = self.rng.pick(&["thinkingLevel", "thinking_level"]);
            fields.push((key, self.loose_choice(LEVELS)));
        }
        self.object(fields)
    }

    /// A temperature or top P: a finite number, as a number or text.
    fn sampling(&mut self) -> Value {
        match self.rng.below(10) {
            0..=7 => num(self.rng.pick(FINITE)),
            8 => self.one_of(&[json!("0.7"), json!("abc"), json!("")]),
            _ => self.one_of(&[Value::Null, json!(true), json!({})]),
        }
    }

    fn thinking_config(&mut self) -> Value {
        if self.rng.chance(5) {
            return self.one_of(&[json!("high"), Value::Null, json!([1])]);
        }
        let mut fields = Vec::new();
        match self.rng.below(10) {
            0..=3 => fields.push(("thinkingBudget", num(self.rng.pick(BUDGETS)))),
            4 => fields.push(("thinking_budget", num(self.rng.pick(BUDGETS)))),
            5..=7 => fields.push(("thinkingLevel", self.loose_choice(LEVELS))),
            8 => fields.push(("thinking_level", self.loose_choice(LEVELS))),
            _ => {
                fields.push(("thinkingLevel", self.loose_choice(LEVELS)));
                fields.push(("thinkingBudget", num(self.rng.pick(BUDGETS))));
            }
        }
        if self.rng.chance(40) {
            fields.push(("includeThoughts", self.bool_like()));
        }
        self.object(fields)
    }

    // --- Responses ---

    /// A Gemini request declaring the tools a Claude request declares, as
    /// the Codex stream generator builds its calls from those.
    fn codex_original(&mut self, claude: &Value) -> Value {
        let declarations: Vec<Value> = claude["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tool| tool.get("name"))
            .map(|name| json!({ "name": name, "parameters": { "type": "OBJECT" } }))
            .collect();
        let mut fields = vec![(
            "contents",
            json!([{ "role": "user", "parts": [{ "text": "hi" }] }]),
        )];
        if !declarations.is_empty() {
            let tools = match self.rng.below(6) {
                0 => declarations
                    .into_iter()
                    .map(|declaration| json!({ "functionDeclarations": [declaration] }))
                    .collect(),
                1 => json!([{ "googleSearch": {} }, { "functionDeclarations": declarations }]),
                2 => json!([{ "function_declarations": declarations }]),
                _ => json!([{ "functionDeclarations": declarations }]),
            };
            fields.push(("tools", tools));
        }
        self.object(fields)
    }
}

/// Numbers for a schema, all finite. Negative zero is `-0.0`: `num` reads
/// `-0` as `0`.
const FINITE_SCHEMA: &[&str] = &["0", "1", "0.5", "1.50", "-7", "1e3", "42", "-0.0"];

/// A Codex stream line with the arguments upstream would copy as broken
/// JSON replaced, and `created_at` set on the response it carries.
fn codex_line(line: &str, created_at: Option<&Value>) -> String {
    let Some(data) = line.strip_prefix("data:") else {
        return line.to_owned();
    };
    match codex_event(data.trim(), created_at) {
        Some(event) => format!("data: {event}"),
        None => line.to_owned(),
    }
}

/// The Codex event in `text`, changed as [`codex_line`] says, or `None` if
/// it needs no change.
fn codex_event(text: &str, created_at: Option<&Value>) -> Option<Value> {
    let mut event: Value = serde_json::from_str(text).ok()?;
    let mut changed = repair_arguments(&mut event);
    if let (Some(created_at), Some(Value::Object(response))) =
        (created_at, event.get_mut("response"))
    {
        response.insert("created_at".into(), created_at.clone());
        changed = true;
    }
    changed.then_some(event)
}

/// Replaces with `{}` every `arguments` string that upstream reads as an
/// object, as it starts with `{`, but that isn't JSON. Upstream copies those
/// as they are, which makes its output invalid JSON.
fn repair_arguments(value: &mut Value) -> bool {
    match value {
        Value::Object(fields) => {
            let mut changed = false;
            for (key, field) in fields.iter_mut() {
                if key == "arguments" && field.as_str().is_some_and(breaks_upstream) {
                    *field = "{}".into();
                    changed = true;
                } else {
                    changed |= repair_arguments(field);
                }
            }
            changed
        }
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |changed, item| repair_arguments(item) | changed),
        _ => false,
    }
}

/// Whether gjson reads `arguments` as an object, skipping leading bytes up
/// to a space, though it isn't JSON.
fn breaks_upstream(arguments: &str) -> bool {
    arguments
        .trim_start_matches(|c: char| c <= ' ')
        .starts_with('{')
        && serde_json::from_str::<Value>(arguments).is_err()
}

/// Whether upstream, reading these Claude stream lines, finishes a tool call
/// whose input isn't JSON. It gathers each block's `partial_json` by the
/// block's index, as an integer, and at the block's stop writes the input,
/// trimmed, as it is.
fn writes_invalid_input(lines: &[String]) -> bool {
    let mut names: HashMap<i64, String> = HashMap::new();
    let mut inputs: HashMap<i64, String> = HashMap::new();
    for line in lines {
        let Some(event) = claude_data(line) else {
            continue;
        };
        let index = gjson_int(event.get("index"));
        match event.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                let block = &event["content_block"];
                if block["type"] == "tool_use"
                    && let Some(name) = block.get("name")
                {
                    names.insert(index, gjson_string(name));
                }
            }
            Some("content_block_delta") if event["delta"]["type"] == "input_json_delta" => {
                let input = inputs.entry(index).or_default();
                if let Some(partial) = event["delta"].get("partial_json") {
                    input.push_str(&gjson_string(partial));
                }
            }
            Some("content_block_stop") => {
                let named = names.get(&index).is_some_and(|name| !name.is_empty());
                let input = inputs.get(&index).map_or("", |input| input.trim());
                if named || !input.is_empty() {
                    if !input.is_empty() && serde_json::from_str::<Value>(input).is_err() {
                        return true;
                    }
                    inputs.remove(&index);
                    names.remove(&index);
                }
            }
            _ => {}
        }
    }
    false
}

/// A Claude stream or body line with any `partial_json` emptied.
fn empty_input(line: &str) -> String {
    let text = line.trim_end_matches('\r');
    let ending = &line[text.len()..];
    let Some(mut event) = claude_data(text) else {
        return line.to_owned();
    };
    match event.pointer_mut("/delta/partial_json") {
        Some(partial) => *partial = "".into(),
        None => return line.to_owned(),
    }
    format!("data: {event}{ending}")
}

/// The event on a Claude stream or body line, as upstream finds it: after
/// `data:` at the line's start, trimmed.
fn claude_data(line: &str) -> Option<Value> {
    let data = line.trim_end_matches('\r').strip_prefix("data:")?;
    serde_json::from_str(data.trim()).ok()
}

/// gjson's `Int()`: a number truncated, text parsed, `true` as 1, anything
/// else 0.
fn gjson_int(value: Option<&Value>) -> i64 {
    let parse = |text: &str| {
        text.parse::<i64>()
            .ok()
            .or_else(|| text.parse::<f64>().ok().map(|float| float as i64))
            .unwrap_or(0)
    };
    match value {
        Some(Value::Number(number)) => parse(&number.to_string()),
        Some(Value::String(text)) => parse(text),
        Some(Value::Bool(true)) => 1,
        _ => 0,
    }
}

/// gjson's `String()`: text as it is, `null` as nothing, anything else as
/// its JSON.
fn gjson_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translator::Translator;

    #[test]
    fn cases_are_reproducible_and_valid_json() {
        let first = request_cases(7, 200);
        let second = request_cases(7, 200);
        for (a, b) in first.iter().zip(&second) {
            assert_eq!(a.request, b.request);
            assert_eq!(a.options, b.options);
            serde_json::from_str::<Value>(&a.request).expect("generated request is valid JSON");
        }
        type Builder = fn(u64, usize) -> (Vec<Case>, Vec<Case>);
        let builders: [Builder; 3] = [codex_event_cases, claude_event_cases, openai_event_cases];
        for build in builders {
            let (streams, finals) = build(7, 200);
            let (again, finals_again) = build(7, 200);
            for (a, b) in streams.iter().zip(&again) {
                assert_eq!(a.request, b.request);
                assert_eq!(a.events, b.events);
            }
            for (a, b) in finals.iter().zip(&finals_again) {
                assert_eq!(a.events, b.events);
            }
        }
    }

    #[test]
    fn broken_arguments_are_replaced() {
        assert!(breaks_upstream("{\"unterminated\":"));
        assert!(breaks_upstream(" {\"a\":1}{\"b\":2}"));
        assert!(!breaks_upstream(" {\"padded\": true} "));
        assert!(!breaks_upstream("not json"));
        let line = r#"data: {"type":"x","item":{"arguments":"{\"a\":"}} "#;
        assert_eq!(
            codex_line(line, None),
            r#"data: {"type":"x","item":{"arguments":"{}"}}"#
        );
        let line = r#"data: {"type":"x","item":{"arguments":"{\"a\":1}"}}"#;
        assert_eq!(codex_line(line, None), line);
    }

    #[test]
    fn claude_input_is_checked_as_upstream_gathers_it() {
        let lines = |inputs: &[(&str, &str)]| -> Vec<String> {
            let mut lines = vec![
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","name":"f"}}"#.to_owned(),
            ];
            for (index, partial) in inputs {
                let delta = json!({ "type": "content_block_delta", "index": num(index), "delta": { "type": "input_json_delta", "partial_json": partial } });
                lines.push(format!("data: {delta}"));
            }
            lines.push(r#"data: {"type":"content_block_stop","index":0}"#.to_owned());
            lines
        };
        assert!(!writes_invalid_input(&lines(&[
            ("0", "{\"a\":"),
            ("0", "1}")
        ])));
        assert!(writes_invalid_input(&lines(&[("0", "{\"a\":")])));
        // A string index reads as its number; one that isn't stopped stays out.
        assert!(writes_invalid_input(&lines(&[("\"0\"", "not json")])));
        assert!(!writes_invalid_input(&lines(&[("1", "not json")])));
        assert_eq!(
            empty_input("data: {\"delta\":{\"partial_json\":\"{\"}}\r"),
            "data: {\"delta\":{\"partial_json\":\"\"}}\r"
        );
    }

    /// Guards against a generator that never reaches the translators' branches.
    #[test]
    fn cases_cover_the_translators_branches() {
        let outputs = |translator: Translator, cases: &[Case]| -> Vec<String> {
            cases
                .iter()
                .map(|case| {
                    translator
                        .run_rust(case)
                        .expect("cases translate")
                        .to_string()
                })
                .collect()
        };
        let check = |outputs: &[String], needles: &[&str]| {
            for needle in needles {
                let count = outputs
                    .iter()
                    .filter(|output| output.contains(needle))
                    .count();
                assert!(count >= 20, "{needle} in {count} outputs");
            }
        };

        let requests = request_cases(1, 2000);
        check(
            &outputs(Translator::CodexGeminiRequest, &requests),
            &[
                r#""type":"function_call""#,
                r#""type":"function_call_output""#,
                r#""type":"input_image""#,
                r#""type":"input_file""#,
                r#""role":"developer""#,
                r#""effort":"high""#,
                r#""service_tier":"priority""#,
                r#""tool_choice":{"#,
            ],
        );
        check(
            &outputs(Translator::ClaudeGeminiRequest, &requests),
            &[
                r#""type":"tool_use""#,
                r#""type":"tool_result""#,
                r#""type":"image""#,
                r#""type":"adaptive""#,
                r#""budget_tokens":"#,
                r#""user_id":"#,
                r#""stop_sequences":"#,
            ],
        );
        check(
            &outputs(Translator::OpenAIGeminiRequest, &requests),
            &[
                r#""tool_calls":["#,
                r#""role":"tool""#,
                r#""role":"system""#,
                r#""reasoning_effort":"#,
                r#""image_url":"#,
                r#""temperature":"#,
            ],
        );

        let (streams, finals) = codex_event_cases(1, 2000);
        check(
            &outputs(Translator::CodexGeminiStream, &streams),
            &[
                "functionCall",
                "thought",
                "usageMetadata",
                "2023-11-14T22:13:20Z",
            ],
        );
        check(
            &outputs(Translator::CodexGeminiNonStream, &finals),
            &["functionCall", "finishReason"],
        );
        let (streams, finals) = claude_event_cases(1, 2000);
        check(
            &outputs(Translator::ClaudeGeminiStream, &streams),
            &["functionCall", "thought", "usageMetadata"],
        );
        check(
            &outputs(Translator::ClaudeGeminiNonStream, &finals),
            &["functionCall", "thought", "finishReason"],
        );
        let (streams, finals) = openai_event_cases(1, 2000);
        check(
            &outputs(Translator::OpenAIGeminiStream, &streams),
            &["functionCall", "thought", "usageMetadata", "outil_m"],
        );
        check(
            &outputs(Translator::OpenAIGeminiNonStream, &finals),
            &["functionCall", "finishReason"],
        );
    }
}
