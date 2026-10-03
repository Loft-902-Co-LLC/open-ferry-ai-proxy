//! Seeded random input for the translators to a Gemini upstream.
//!
//! Gemini requests take their `contents` from the signature generator
//! ([`super::signature`]): roles to fix, thought signatures to sanitize, and
//! function responses, some of which lose their names here. Now and then
//! `contents` is an object, which upstream walks key by key. Added to them
//! are tools declared under either key, with `parameters` or
//! `parametersJsonSchema`, a generation config with a `responseSchema`, a
//! system instruction and safety settings.
//!
//! Claude requests are the Claude request generator's ([`super::cases`])
//! with sampling settings added, for a Gemini model. Chat Completions
//! requests are the Chat Completions generator's ([`super::chat`]) with what
//! only this translator reads added: sampling and output settings,
//! modalities, image settings, Google's tools, strict tools, `allowed_tools`
//! choices, reasoning text, thought signatures on tool calls, and video,
//! audio, file and image parts. Every tool call's arguments are made JSON:
//! upstream copies them unchecked, so other text makes its output invalid
//! JSON (see the translator's deviations).
//!
//! Responses are Gemini streams and whole responses answering a request that
//! declares tools by names that need sanitizing, in other cases, padded, or
//! with leading underscores. They hold text, thoughts, signatures alone,
//! function calls under sanitized, declared or undeclared names, calls
//! continued without a name, inline data, transcripts, several candidates,
//! finish reasons, creation times and usage. Streams for the passthrough and
//! Chat Completions translators come as `data:` lines or bare JSON, with
//! blank lines, comments and `[DONE]`; those for the Claude translator as
//! bare JSON then, usually, `[DONE]`, as upstream's executor passes them.

use std::ops::{Deref, DerefMut};

use serde_json::{Value, json};

use super::{Rng, escape_text, num, to_object};
use crate::cases::Case;

/// Builds `count` random Gemini requests, each asking for a stream or not.
pub fn request_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed, index);
            let request = generator.gemini_request();
            let text = generator.render(&request);
            let model = generator.rng.pick(MODELS);
            let stream = generator.rng.chance(50);
            Case::new(format!("random-{seed}-{index}"), model, text)
                .with_options(json!({ "stream": stream }))
        })
        .collect()
}

/// Builds `count` random Gemini streams for the passthrough translator, and
/// a non-streaming case from a whole response for each.
pub fn event_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed.rotate_left(9), index);
            let calls = generator.calls(&[], false);
            let chunks = generator.chunks(&calls);
            let lines = generator.data_lines(&chunks, &calls);
            let body = match generator.rng.below(40) {
                0 => "not json".to_owned(),
                _ => generator.body(&calls),
            };
            let case = |events| Case::response(format!("random-{seed}-{index}"), "", events);
            (case(lines), case(vec![body]))
        })
        .unzip()
}

/// Builds `count` random Claude requests for a Gemini upstream, each asking
/// for a stream or not.
pub fn claude_request_cases(seed: u64, count: usize) -> Vec<Case> {
    super::cases(seed.rotate_left(37), count)
        .into_iter()
        .enumerate()
        .map(|(index, case)| {
            let mut generator = Generator::new(seed.rotate_left(11), index as u64);
            let mut request: Value =
                serde_json::from_str(&case.request).expect("generated requests are JSON");
            generator.claude_fields(&mut request);
            let text = generator.render(&request);
            let model = generator.rng.pick(MODELS);
            let stream = generator.rng.chance(50);
            Case::new(case.name, model, text).with_options(json!({ "stream": stream }))
        })
        .collect()
}

/// Builds `count` random Gemini streams answering random Claude requests,
/// and a non-streaming case from a whole response for each.
pub fn claude_event_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed.rotate_left(13), index);
            let (request, declared) = generator.original_request(false);
            let calls = generator.calls(&declared, true);
            let chunks = generator.chunks(&calls);
            let lines = generator.bare_lines(&chunks);
            let body = generator.body(&calls);
            let case = |events| Case::response(format!("random-{seed}-{index}"), &request, events);
            (case(lines), case(vec![body]))
        })
        .unzip()
}

/// Builds `count` random Chat Completions requests for a Gemini upstream,
/// each asking for a stream or not.
pub fn chat_request_cases(seed: u64, count: usize) -> Vec<Case> {
    super::chat::request_cases(seed.rotate_left(41), count)
        .into_iter()
        .enumerate()
        .map(|(index, case)| {
            let mut generator = Generator::new(seed.rotate_left(19), index as u64);
            let mut request: Value =
                serde_json::from_str(&case.request).expect("generated requests are JSON");
            generator.chat_fields(&mut request);
            json_arguments(&mut request);
            let text = generator.render(&request);
            let model = generator.rng.pick(MODELS);
            let stream = generator.rng.chance(50);
            Case::new(case.name, model, text).with_options(json!({ "stream": stream }))
        })
        .collect()
}

/// Builds `count` random Gemini streams answering random Chat Completions
/// requests, and a non-streaming case from a whole response for each.
pub fn chat_event_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed.rotate_left(23), index);
            let (request, declared) = generator.original_request(true);
            let calls = generator.calls(&declared, false);
            let chunks = generator.chunks(&calls);
            let lines = generator.data_lines(&chunks, &calls);
            let body = generator.body(&calls);
            let case = |events| Case::response(format!("random-{seed}-{index}"), &request, events);
            (case(lines), case(vec![body]))
        })
        .unzip()
}

/// Makes the `arguments` of every tool call in a Chat Completions request
/// JSON text, `{}` where it isn't. Upstream copies them into its output
/// unchecked; we leave out arguments that aren't JSON.
pub fn json_arguments(request: &mut Value) {
    let Some(Value::Array(messages)) = request.get_mut("messages") else {
        return;
    };
    for message in messages {
        let Some(Value::Array(calls)) = message.get_mut("tool_calls") else {
            continue;
        };
        for call in calls {
            let Some(Value::Object(function)) = call.get_mut("function") else {
                continue;
            };
            let json = match function.get("arguments") {
                Some(Value::String(text)) => serde_json::from_str::<Value>(text).is_ok(),
                None | Some(Value::Null) => false,
                Some(_) => true,
            };
            if !json {
                function.insert("arguments".to_owned(), json!("{}"));
            }
        }
    }
}

/// Models a client may ask for: Gemini's, with and without thinking support
/// in the catalog, and ones the catalog doesn't know.
const MODELS: &[&str] = &[
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-2.5-flash-lite",
    "gemini-3-pro-preview",
    "gemini-3.1-pro",
    "gemini-2.0-flash",
    "claude-sonnet-4-5",
    "unknown-model",
    "",
];

/// Tool names a client declares: some valid for Gemini, some it sanitizes
/// (spaces, other scripts, a leading digit, a slash, over 64 bytes), some
/// with leading underscores or padding, and some whose case maps differ
/// between Unicode's tables.
const TOOL_NAMES: &[&str] = &[
    "Bash",
    "Read",
    "TodoWrite",
    "mcp__github__get_me",
    "web search",
    "outil_météo",
    "日本語ツール",
    "1st_tool",
    "__Edit",
    "Write ",
    "tool/with/slash",
    "a.b:c-d",
    "ΑΣ",
    "İstanbul",
    "\u{212a}elvin",
    "mcp__server__a_very_long_tool_name_that_goes_well_past_sixty_four_bytes",
    "",
];

/// Thought signatures as Gemini sends them, and values that aren't.
const SIGNATURES: &[&str] = &[
    "c2lnbmF0dXJl",
    "EqQBCkgIBxABGAIqQEp0",
    "sig/with+chars=",
    "",
    "skip_thought_signature_validator",
];

/// Token counts, within `i64`'s range: Go's conversion of a float beyond it
/// depends on the CPU.
const COUNTS: &[&str] = &[
    "0",
    "1",
    "7",
    "12",
    "100",
    "2048",
    "-3",
    "1.5",
    "\"12\"",
    "null",
    "9007199254740993",
];

/// Creation times: RFC 3339 with and without fractions and offsets, and
/// what Go's parser accepts beyond it or rejects.
const CREATE_TIMES: &[&str] = &[
    "2025-01-02T03:04:05Z",
    "2025-01-02T03:04:05.123456789Z",
    "2025-06-30T23:59:59.5+02:00",
    "2024-02-29T12:00:00-07:30",
    "2025-01-02T3:04:05Z",
    "2025-01-02T03:04:05,25Z",
    "2025-02-29T00:00:00Z",
    "2025-01-02T24:00:00Z",
    "2025-01-02T03:04:60Z",
    "2025-01-02 03:04:05Z",
    "2025-01-02T03:04:05",
    "2025-01-02T03:04:05+24:60",
    "2025-01-02T03:04:05+25:00",
    "2025-01-02T03:04:05.Z",
    "1969-12-31T23:59:59Z",
    "0001-01-01T00:00:00Z",
    "2025-01-02",
    "",
    "not a time",
];

/// `data:` URLs as clients send them, some missing a part upstream needs.
const DATA_URLS: &[&str] = &[
    "data:image/png;base64,iVBORw0KGgo=",
    "data:video/mp4;base64,AAAAIGZ0eXA=",
    "data:image/jpeg;base64,",
    "data:;base64,QQ==",
    "data:image/png,abc",
    "data:x;y",
    "data:image/png;charset=utf-8;base64,QQ",
    "data:é;base64,QQ",
    "https://example.com/a.png",
];

const AUDIO_FORMATS: &[&str] = &[
    "wav",
    "mp3",
    "ogg",
    "flac",
    "aac",
    "webm",
    "pcm16",
    "g711_ulaw",
    "g711_alaw",
    "m4a",
    "",
];

const FILENAMES: &[&str] = &[
    "a.pdf",
    "notes.txt",
    "image.PNG",
    "data.json",
    "archive.unknown",
    "noext",
    "",
];

const FILE_DATA: &[&str] = &[
    "data:application/pdf;base64,JVBERi0=",
    "JVBERi0=",
    "data:text/plain,hello",
    "",
    "data:;base64,QQ==",
];

/// The Claude request generator, for its leaf values and Gemini contents.
struct Generator {
    base: super::Generator,
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
                rng: Rng(seed.rotate_left(27) ^ index.wrapping_mul(0x9FB2_1C65_1E98_DF25)),
                tool_names: Vec::new(),
                tool_use_ids: Vec::new(),
            },
        }
    }

    // --- Gemini requests ---

    fn gemini_request(&mut self) -> Value {
        let mut fields = Vec::new();
        if self.rng.chance(92) {
            fields.push(("contents", self.request_contents()));
        }
        if self.rng.chance(50) {
            fields.push(("tools", self.gemini_tools()));
        }
        if self.rng.chance(40) {
            fields.push(("generationConfig", self.generation_config()));
        }
        if self.rng.chance(40) {
            let key = if self.rng.chance(80) {
                "systemInstruction"
            } else {
                "system_instruction"
            };
            let text = self.text();
            fields.push((key, json!({ "role": "user", "parts": [{ "text": text }] })));
        }
        if self.rng.chance(25) {
            let settings = self.one_of(&[
                json!([{ "category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_NONE" }]),
                json!([]),
                Value::Null,
                json!("x"),
            ]);
            let key = if self.rng.chance(85) {
                "safetySettings"
            } else {
                "safety_settings"
            };
            fields.push((key, settings));
        }
        if self.rng.chance(15) {
            fields.push((
                "toolConfig",
                json!({ "functionCallingConfig": { "mode": "AUTO" } }),
            ));
        }
        if self.rng.chance(10) {
            fields.push(("cachedContent", json!("cachedContents/abc123")));
        }
        if self.rng.chance(10) {
            fields.push(("model", json!("gemini-2.5-pro")));
        }
        self.rng.shuffle(&mut fields);
        to_object(fields)
    }

    /// The signature generator's contents, with some function responses
    /// unnamed, given now and then as an object or not as contents at all.
    fn request_contents(&mut self) -> Value {
        let mut contents = self.gemini_contents();
        let Value::Array(items) = &mut contents else {
            return contents;
        };
        for content in items.iter_mut() {
            self.unname_responses(content);
        }
        match self.rng.below(20) {
            0..=15 => contents,
            16 | 17 => {
                let Value::Array(items) = contents else {
                    unreachable!("matched as an array above");
                };
                let numeric = self.rng.chance(70);
                let fields = items
                    .into_iter()
                    .enumerate()
                    .map(|(index, item)| {
                        let key = if numeric {
                            index.to_string()
                        } else {
                            ["a", "b", "c", "d", "e", "f", "g"][index % 7].to_owned()
                        };
                        (key, item)
                    })
                    .collect();
                Value::Object(fields)
            }
            18 => self.one_of(&[json!({}), Value::Null, json!("contents"), json!(5)]),
            _ => json!([]),
        }
    }

    /// Takes the names off some of a turn's function responses, as some
    /// clients send them.
    fn unname_responses(&mut self, content: &mut Value) {
        let Some(Value::Array(parts)) = content.get_mut("parts") else {
            return;
        };
        for part in parts {
            let Some(Value::Object(response)) = part.get_mut("functionResponse") else {
                continue;
            };
            match self.rng.below(10) {
                0..=2 => {
                    response.insert("name".to_owned(), json!(""));
                }
                3 => {
                    response.insert("name".to_owned(), json!("  "));
                }
                4 => {
                    response.shift_remove("name");
                }
                5 => {
                    response.insert("name".to_owned(), Value::Null);
                }
                _ => {}
            }
        }
    }

    fn gemini_tools(&mut self) -> Value {
        if self.rng.chance(5) {
            return self.one_of(&[json!({}), json!("tools"), Value::Null]);
        }
        let count = 1 + self.rng.below(3);
        Value::Array((0..count).map(|_| self.gemini_tool()).collect())
    }

    fn gemini_tool(&mut self) -> Value {
        match self.rng.below(20) {
            0..=8 => json!({ "functionDeclarations": self.declarations() }),
            9..=11 => json!({ "function_declarations": self.declarations() }),
            12 | 13 => {
                let fields = vec![
                    ("function_declarations", self.declarations()),
                    ("functionDeclarations", self.declarations()),
                ];
                self.object(fields)
            }
            14 => json!({ "googleSearch": {} }),
            15 => json!({ "codeExecution": {} }),
            16 => json!({ "functionDeclarations": self.declarations(), "googleSearch": {} }),
            17 => self.one_of(&[
                json!({ "functionDeclarations": "x" }),
                json!({ "functionDeclarations": null }),
            ]),
            _ => self.one_of(&[json!(5), json!("tool"), Value::Null, json!({})]),
        }
    }

    fn declarations(&mut self) -> Value {
        let count = self.rng.below(4);
        Value::Array((0..count).map(|_| self.declaration()).collect())
    }

    fn declaration(&mut self) -> Value {
        if self.rng.chance(5) {
            return self.one_of(&[json!(5), Value::Null, json!("declaration")]);
        }
        let name = self
            .rng
            .pick(&["get_weather", "search", "Bash", "mcp__github__get_me"]);
        let mut fields = vec![("name", json!(name))];
        if self.rng.chance(50) {
            fields.push(("description", self.text().into()));
        }
        let schema = || json!({ "type": "object", "properties": { "city": { "type": "string" } }, "required": ["city"] });
        match self.rng.below(10) {
            0..=5 => fields.push(("parameters", schema())),
            6 => fields.push(("parametersJsonSchema", schema())),
            7 => {
                fields.push(("parametersJsonSchema", json!({ "type": "object" })));
                fields.push(("parameters", schema()));
            }
            8 => fields.push(("parameters", self.one_of(&[Value::Null, json!("x")]))),
            _ => {}
        }
        self.object(fields)
    }

    fn generation_config(&mut self) -> Value {
        if self.rng.chance(5) {
            return self.one_of(&[json!("config"), Value::Null, json!([])]);
        }
        let mut fields = Vec::new();
        if self.rng.chance(55) {
            fields.push((
                "responseSchema",
                json!({ "type": "OBJECT", "properties": { "answer": { "type": "STRING" } } }),
            ));
        }
        if self.rng.chance(15) {
            fields.push(("responseJsonSchema", json!({ "type": "object" })));
        }
        if self.rng.chance(30) {
            fields.push(("temperature", self.number()));
        }
        if self.rng.chance(30) {
            let thinking = self.one_of(&[
                json!({ "thinkingBudget": 1024 }),
                json!({ "includeThoughts": true, "thinkingLevel": "high" }),
            ]);
            fields.push(("thinkingConfig", thinking));
        }
        if self.rng.chance(20) {
            fields.push(("responseMimeType", json!("application/json")));
        }
        self.object(fields)
    }

    // --- Claude requests ---

    /// Adds the sampling settings the Gemini translator reads to a Claude
    /// request, now and then as something other than a number.
    fn claude_fields(&mut self, request: &mut Value) {
        let Value::Object(fields) = request else {
            return;
        };
        for key in ["temperature", "top_p", "top_k"] {
            if self.rng.chance(25) {
                let value = self.sampling();
                fields.insert(key.to_owned(), value);
            }
        }
    }

    fn sampling(&mut self) -> Value {
        match self.rng.below(10) {
            0..=6 => self.number(),
            7 => json!("0.5"),
            8 => Value::Null,
            _ => json!(true),
        }
    }

    // --- Chat Completions requests ---

    /// Adds what only the Gemini translator reads to a Chat Completions
    /// request.
    fn chat_fields(&mut self, request: &mut Value) {
        let Value::Object(fields) = request else {
            return;
        };
        if let Some(Value::Array(messages)) = fields.get_mut("messages") {
            for message in messages.iter_mut() {
                self.chat_message(message);
            }
            if self.rng.chance(30) {
                let at = self.rng.below(messages.len() + 1);
                let message = self.media_message();
                messages.insert(at, message);
            }
        }
        if let Some(Value::Array(tools)) = fields.get_mut("tools") {
            if self.rng.chance(30) {
                let at = self.rng.below(tools.len() + 1);
                let tool = self.extra_tool();
                tools.insert(at, tool);
            }
            if self.rng.chance(15) {
                for tool in tools.iter_mut() {
                    if let Value::Object(tool) = tool
                        && self.rng.chance(50)
                    {
                        let strict =
                            self.one_of(&[json!(true), json!(true), json!(false), json!("true")]);
                        tool.insert("strict".to_owned(), strict);
                    }
                }
            }
        }
        let declared: Vec<Value> = match fields.get("tools") {
            Some(Value::Array(tools)) => tools
                .iter()
                .filter_map(|tool| tool.pointer("/function/name").cloned())
                .collect(),
            _ => Vec::new(),
        };
        if self.rng.chance(25) {
            let choice = self.chat_tool_choice(&declared);
            fields.insert("tool_choice".to_owned(), choice);
        }
        for (key, chance) in [
            ("top_p", 20),
            ("top_k", 15),
            ("temperature", 15),
            ("max_tokens", 15),
            ("max_completion_tokens", 15),
        ] {
            if self.rng.chance(chance) {
                let value = self.sampling();
                fields.insert(key.to_owned(), value);
            }
        }
        if self.rng.chance(10) {
            let n =
                num(self
                    .rng
                    .pick(&["1", "2", "3", "\"2\"", "0", "2.5", "-1", "9007199254740993"]));
            fields.insert("n".to_owned(), n);
        }
        if self.rng.chance(10) {
            let config = self.generation_config();
            fields.insert("generationConfig".to_owned(), config);
        }
        if self.rng.chance(15) {
            let modalities = if self.rng.chance(90) {
                let count = self.rng.below(4);
                Value::Array(
                    (0..count)
                        .map(|_| {
                            self.loose_choice(&["text", "image", "TEXT", "Image", "audio", ""])
                        })
                        .collect(),
                )
            } else {
                json!("text")
            };
            fields.insert("modalities".to_owned(), modalities);
        }
        if self.rng.chance(10) {
            let config = if self.rng.chance(90) {
                let mut config = Vec::new();
                if self.rng.chance(70) {
                    config.push((
                        "aspect_ratio",
                        self.one_of(&[json!("16:9"), json!("1:1"), json!(5)]),
                    ));
                }
                if self.rng.chance(60) {
                    config.push((
                        "image_size",
                        self.one_of(&[json!("1K"), json!("2K"), Value::Null]),
                    ));
                }
                self.object(config)
            } else {
                json!("x")
            };
            fields.insert("image_config".to_owned(), config);
        }
        if self.rng.chance(25) {
            let format = self.chat_response_format();
            fields.insert("response_format".to_owned(), format);
        }
        if self.rng.chance(10) {
            let effort = self.one_of(&[
                json!("auto"),
                json!(" AUTO "),
                json!("Auto"),
                json!("none"),
                json!("high"),
                json!(""),
            ]);
            fields.insert("reasoning_effort".to_owned(), effort);
        }
        if self.rng.chance(8) {
            let parallel = self.one_of(&[json!(false), json!(true), json!("false"), json!(0)]);
            fields.insert("parallel_tool_calls".to_owned(), parallel);
        }
    }

    /// Adds reasoning text and thought signatures to an assistant message.
    fn chat_message(&mut self, message: &mut Value) {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        let Value::Object(fields) = message else {
            return;
        };
        if self.rng.chance(25) {
            let reasoning = match self.rng.below(5) {
                0..=2 => self.text().into(),
                3 => json!(""),
                _ => self.one_of(&[json!(5), Value::Null]),
            };
            fields.insert("reasoning_content".to_owned(), reasoning);
        }
        if let Some(Value::Array(calls)) = fields.get_mut("tool_calls") {
            for call in calls {
                if self.rng.chance(30) {
                    self.call_signature(call);
                }
            }
        }
    }

    /// Gives a tool call a thought signature, in any of the places upstream
    /// looks.
    fn call_signature(&mut self, call: &mut Value) {
        let Value::Object(fields) = call else {
            return;
        };
        let signature: Value = if self.rng.chance(85) {
            self.signature_text().into()
        } else {
            self.one_of(&[json!(5), Value::Null, json!("")])
        };
        let extra = json!({ "google": { "thought_signature": signature } });
        match self.rng.below(4) {
            0 => {
                fields.insert("extra_content".to_owned(), extra);
            }
            1 => {
                if let Some(Value::Object(function)) = fields.get_mut("function") {
                    function.insert("extra_content".to_owned(), extra);
                }
            }
            2 => {
                fields.insert("thoughtSignature".to_owned(), signature);
            }
            _ => {
                fields.insert("thought_signature".to_owned(), signature);
            }
        }
    }

    /// A message with a part only this translator reads, or reads its own
    /// way.
    fn media_message(&mut self) -> Value {
        let part = match self.rng.below(10) {
            0 | 1 => {
                json!({ "type": "video_url", "video_url": { "url": self.rng.pick(DATA_URLS) } })
            }
            2 | 3 => {
                json!({ "type": "input_audio", "input_audio": { "data": "UklGRg==", "format": self.rng.pick(AUDIO_FORMATS) } })
            }
            4 | 5 => {
                json!({ "type": "file", "file": { "filename": self.rng.pick(FILENAMES), "file_data": self.rng.pick(FILE_DATA) } })
            }
            6 | 7 => {
                json!({ "type": "image_url", "image_url": { "url": self.rng.pick(DATA_URLS) } })
            }
            _ => json!({ "type": "text", "text": self.text() }),
        };
        let role = self
            .rng
            .pick(&["user", "user", "assistant", "system", "developer"]);
        let content = if self.rng.chance(85) {
            json!([part])
        } else {
            part
        };
        json!({ "role": role, "content": content })
    }

    /// Google's tools, and function tools whose schema is already in
    /// Gemini's key.
    fn extra_tool(&mut self) -> Value {
        match self.rng.below(9) {
            0 | 1 => json!({ "google_search": {} }),
            2 => {
                json!({ "type": "function", "google_search": { "dynamic_retrieval_config": { "mode": "MODE_DYNAMIC" } } })
            }
            3 => json!({ "code_execution": {} }),
            4 => json!({ "url_context": {} }),
            5 => json!({ "google_search": {}, "url_context": {} }),
            6 => {
                json!({ "type": "function", "function": { "name": "lookup", "parametersJsonSchema": self.one_of(&[json!([1]), json!("x"), json!({ "type": "object", "properties": { "q": { "type": "string" } } })]) } })
            }
            7 => {
                json!({ "type": "function", "strict": true, "function": { "name": "strict_tool", "parameters": { "type": "object" } } })
            }
            _ => {
                json!({ "type": "function", "function": { "name": "strict_inside", "strict": true, "parameters": { "type": "object", "properties": {} } } })
            }
        }
    }

    fn chat_tool_choice(&mut self, declared: &[Value]) -> Value {
        let name = if !declared.is_empty() && self.rng.chance(80) {
            self.rng.pick(declared)
        } else {
            self.one_of(&[json!("get_weather"), json!("lookup"), json!("")])
        };
        let mode = self.one_of(&[
            json!("required"),
            json!("any"),
            json!("auto"),
            json!(" ANY "),
            json!(""),
            json!(5),
        ]);
        match self.rng.below(9) {
            0 | 1 => {
                json!({ "type": "allowed_tools", "allowed_tools": { "mode": mode, "tools": [{ "type": "function", "function": { "name": name } }] } })
            }
            2 => {
                json!({ "type": "allowed_tools", "mode": mode, "tools": [{ "type": "function", "name": name }] })
            }
            3 => {
                json!({ "type": "allowed_tools", "allowed_tools": { "tools": [{ "function": { "name": " " }, "name": name }] } })
            }
            4 => {
                json!({ "type": "allowed_tools", "allowed_tools": { "mode": mode, "tools": { "type": "function", "function": { "name": name } } } })
            }
            5 => json!({ "type": "function", "function": { "name": name } }),
            6 => json!({ "type": "tool", "name": name }),
            7 => json!({ "type": "function", "function": { "name": " " }, "name": name }),
            _ => self.one_of(&[
                json!("any"),
                json!(" Required "),
                json!({ "type": "AUTO" }),
                json!({ "type": "none" }),
                json!("auto"),
            ]),
        }
    }

    fn chat_response_format(&mut self) -> Value {
        let schema = json!({ "type": "object", "properties": { "answer": { "type": "string" } }, "additionalProperties": false });
        match self.rng.below(8) {
            0 | 1 => json!({ "type": "json_object" }),
            2 | 3 => {
                json!({ "type": "json_schema", "json_schema": { "name": "answer", "strict": true, "schema": schema } })
            }
            4 => json!({ "type": " JSON_SCHEMA ", "json_schema": { "name": "x" } }),
            5 => json!({ "type": "text" }),
            6 => json!({ "type": "json_schema", "json_schema": { "schema": "x" } }),
            _ => self.one_of(&[json!("json_object"), Value::Null, json!({ "type": 5 })]),
        }
    }

    // --- Responses ---

    /// The client's request, as JSON text, declaring tools by the names
    /// returned. A Chat Completions client declares them as functions, or
    /// by `name` alone, which is where upstream looks for names to restore;
    /// a Claude client declares them by `name`.
    fn original_request(&mut self, chat: bool) -> (String, Vec<String>) {
        let mut declared = Vec::new();
        let mut fields = Vec::new();
        if self.rng.chance(80) {
            let count = 1 + self.rng.below(4);
            let tools: Vec<Value> = (0..count)
                .map(|_| {
                    let name = self.rng.pick(TOOL_NAMES).to_owned();
                    declared.push(name.clone());
                    match (chat, self.rng.below(10)) {
                        (false, 0..=7) => {
                            json!({ "name": name, "description": "A tool.", "input_schema": { "type": "object" } })
                        }
                        (false, 8) => json!({ "type": "function", "function": { "name": name } }),
                        (false, _) => json!({ "name": format!(" {name} ") }),
                        (true, 0..=4) => json!({ "name": name }),
                        (true, 5..=8) => {
                            json!({ "type": "function", "function": { "name": name, "parameters": { "type": "object" } } })
                        }
                        (true, _) => {
                            json!({ "type": "function", "name": name, "function": { "name": name } })
                        }
                    }
                })
                .collect();
            let tools = if self.rng.chance(5) {
                self.one_of(&[json!({}), json!("tools"), json!([{ "name": 5 }, 1])])
            } else {
                Value::Array(tools)
            };
            fields.push(("tools", tools));
        }
        let model = if chat {
            "gemini-2.5-pro"
        } else {
            "claude-sonnet-4-5"
        };
        fields.push(("model", json!(model)));
        fields.push(("messages", json!([{ "role": "user", "content": "Hi" }])));
        let request = self.object(fields);
        let text = match self.rng.below(50) {
            0 => String::new(),
            1 => "[]".to_owned(),
            _ => self.render(&request),
        };
        (text, declared)
    }

    /// The names a model might call the declared tools by: as Gemini was
    /// sent them, sanitized, and, for the Claude translator, which also maps
    /// names back by their canonical form, in other cases or with a leading
    /// underscore. Then names never declared.
    fn calls(&mut self, declared: &[String], claude: bool) -> Vec<String> {
        let mut names = Vec::new();
        for name in declared {
            let sanitized = sanitize(name.trim());
            names.push(sanitized.clone());
            let variant = match self.rng.below(if claude { 6 } else { 2 }) {
                0 => name.clone(),
                1 => sanitized.clone(),
                2 => sanitized.to_lowercase(),
                3 => format!("_{sanitized}"),
                4 => sanitized.to_uppercase(),
                _ => format!("__{}", sanitized.to_lowercase()),
            };
            names.push(variant);
        }
        for name in ["get_weather", "Glob", "undeclared tool"] {
            if self.rng.chance(30) {
                names.push(name.to_owned());
            }
        }
        names
    }

    /// The chunks of a Gemini stream. The last usually carries the finish
    /// reasons and usage.
    fn chunks(&mut self, calls: &[String]) -> Vec<Value> {
        let count = 1 + self.rng.below(5);
        let candidates = if self.rng.chance(15) { 2 } else { 1 };
        let model = self.rng.chance(80).then(|| self.model_version());
        let id = self.rng.chance(70).then(|| self.response_id());
        let usage_everywhere = self.rng.chance(15);
        (0..count)
            .map(|index| {
                let last = index + 1 == count;
                self.chunk(calls, candidates, last, usage_everywhere, &model, &id)
            })
            .collect()
    }

    fn chunk(
        &mut self,
        calls: &[String],
        candidates: usize,
        last: bool,
        usage_everywhere: bool,
        model: &Option<Value>,
        id: &Option<Value>,
    ) -> Value {
        let mut fields = Vec::new();
        if self.rng.chance(93) {
            fields.push(("candidates", self.candidates(candidates, calls, last)));
        }
        if (last && self.rng.chance(85)) || usage_everywhere || self.rng.chance(5) {
            fields.push(("usageMetadata", self.usage()));
        }
        if let Some(model) = model
            && self.rng.chance(85)
        {
            fields.push(("modelVersion", model.clone()));
        }
        if let Some(id) = id
            && self.rng.chance(85)
        {
            fields.push(("responseId", id.clone()));
        }
        if self.rng.chance(50) {
            fields.push(("createTime", self.create_time()));
        }
        self.object(fields)
    }

    fn candidates(&mut self, count: usize, calls: &[String], last: bool) -> Value {
        if self.rng.chance(4) {
            return self.one_of(&[json!([]), json!({}), Value::Null, json!("x")]);
        }
        Value::Array(
            (0..count)
                .map(|index| self.candidate(index, count, calls, last))
                .collect(),
        )
    }

    fn candidate(&mut self, index: usize, count: usize, calls: &[String], last: bool) -> Value {
        let mut fields = Vec::new();
        if self.rng.chance(92) {
            let parts = if self.rng.chance(4) {
                self.one_of(&[json!("x"), Value::Null, json!({})])
            } else {
                let count = self.rng.below(4) + usize::from(self.rng.chance(60));
                Value::Array((0..count).map(|_| self.part(calls)).collect())
            };
            let mut content = vec![("parts", parts)];
            if self.rng.chance(80) {
                content.insert(0, ("role", json!("model")));
            }
            fields.push(("content", to_object(content)));
        }
        if (last && self.rng.chance(85)) || self.rng.chance(8) {
            fields.push(("finishReason", self.finish_reason()));
        }
        if count > 1 || self.rng.chance(30) {
            let index = if self.rng.chance(90) {
                json!(index)
            } else {
                self.one_of(&[json!("1"), num("1.0"), json!(7), Value::Null])
            };
            fields.push(("index", index));
        }
        if self.rng.chance(10) {
            fields.push((
                "safetyRatings",
                json!([{ "category": "HARM_CATEGORY_HARASSMENT", "probability": "NEGLIGIBLE" }]),
            ));
        }
        self.object(fields)
    }

    fn part(&mut self, calls: &[String]) -> Value {
        let mut fields: Vec<(&str, Value)> = Vec::new();
        match self.rng.below(100) {
            0..=27 => {
                fields.push(("text", self.text().into()));
                if self.rng.chance(10) {
                    self.with_signature(&mut fields);
                }
            }
            28..=44 => {
                fields.push(("text", self.text().into()));
                let thought = self.one_of(&[
                    json!(true),
                    json!(true),
                    json!(true),
                    json!("true"),
                    json!(false),
                    json!(1),
                ]);
                fields.push(("thought", thought));
                if self.rng.chance(40) {
                    self.with_signature(&mut fields);
                }
            }
            45..=49 => self.with_signature(&mut fields),
            50 | 51 => {
                fields.push(("text", json!("")));
                if self.rng.chance(50) {
                    fields.push(("thought", json!(true)));
                }
                self.with_signature(&mut fields);
            }
            52..=69 => {
                let call = self.function_call(calls, true);
                fields.push(("functionCall", call));
                if self.rng.chance(15) {
                    self.with_signature(&mut fields);
                }
            }
            70..=73 => fields.push(("functionCall", self.function_call(calls, false))),
            74..=83 => fields.push(self.inline_data()),
            84..=88 => {
                let transcript = json!({ "text": self.text() });
                fields.push(("audioTranscription", transcript));
            }
            _ => {
                return self.one_of(&[
                    json!(5),
                    Value::Null,
                    json!("x"),
                    json!({}),
                    json!({ "text": 5 }),
                    json!({ "text": null }),
                    json!({ "text": true }),
                    json!({ "functionCall": null }),
                    json!({ "functionCall": "f" }),
                ]);
            }
        }
        self.object(fields)
    }

    fn with_signature(&mut self, fields: &mut Vec<(&str, Value)>) {
        let signature = if self.rng.chance(95) {
            json!(self.rng.pick(SIGNATURES))
        } else {
            json!(5)
        };
        match self.rng.below(10) {
            0..=7 => fields.push(("thoughtSignature", signature)),
            8 => fields.push(("thought_signature", signature)),
            _ => {
                fields.push(("thoughtSignature", signature));
                fields.push(("thought_signature", json!("c2Vjb25k")));
            }
        }
    }

    /// A function call, by one of `calls` or another name, or with no name,
    /// which continues the call before it.
    fn function_call(&mut self, calls: &[String], named: bool) -> Value {
        let mut fields = Vec::new();
        if named {
            let name = if !calls.is_empty() && self.rng.chance(85) {
                json!(self.rng.pick(calls))
            } else {
                self.one_of(&[json!("get_weather"), json!(""), json!(5)])
            };
            fields.push(("name", name));
        }
        if let Some(args) = self.args() {
            fields.push(("args", args));
        }
        if self.rng.chance(10) {
            fields.push(("id", json!(format!("call_{}", self.alphanumeric(6)))));
        }
        self.object(fields)
    }

    fn args(&mut self) -> Option<Value> {
        Some(match self.rng.below(20) {
            0..=4 => json!({ "city": "Paris" }),
            5 => json!({ "command": "ls -la", "timeout": num("1.50") }),
            6 => json!({ "path": "/tmp/café.txt", "lines": [1, 2, 3] }),
            7 => json!({ "text": "quote \" backslash \\ <tag> & \u{2028}" }),
            8 => json!({ "nested": { "a": [null, true, { "b": num("1e3") }] } }),
            9 | 10 => json!({}),
            11 => json!([1, "two"]),
            12 => json!("a string"),
            13 => self.one_of(&[json!(5), Value::Null, json!(true)]),
            14 => json!({ "finishReason": "STOP" }),
            15 => json!({ "emoji": "🚀", "日本": "語" }),
            _ => return None,
        })
    }

    fn inline_data(&mut self) -> (&'static str, Value) {
        match self.rng.below(8) {
            0..=2 => (
                "inlineData",
                json!({ "mimeType": "image/png", "data": "iVBORw0KGgo=" }),
            ),
            3 => (
                "inline_data",
                json!({ "mime_type": "image/jpeg", "data": "/9j/4AAQ" }),
            ),
            4 => ("inlineData", json!({ "data": "UklGRg==" })),
            5 => (
                "inlineData",
                json!({ "mimeType": "image/webp", "data": "" }),
            ),
            6 => (
                "inlineData",
                json!({ "mimeType": "", "mime_type": "image/gif", "data": "R0lGOD" }),
            ),
            _ => ("inline_data", json!({ "mimeType": "audio/wav", "data": 5 })),
        }
    }

    fn usage(&mut self) -> Value {
        if self.rng.chance(3) {
            return self.one_of(&[json!({}), Value::Null, json!(5)]);
        }
        let mut fields = Vec::new();
        for (key, chance) in [
            ("promptTokenCount", 90),
            ("candidatesTokenCount", 85),
            ("totalTokenCount", 80),
            ("thoughtsTokenCount", 35),
            ("cachedContentTokenCount", 30),
        ] {
            if self.rng.chance(chance) {
                fields.push((key, num(self.rng.pick(COUNTS))));
            }
        }
        if self.rng.chance(10) {
            fields.push((
                "promptTokensDetails",
                json!([{ "modality": "TEXT", "tokenCount": 5 }]),
            ));
        }
        self.object(fields)
    }

    fn finish_reason(&mut self) -> Value {
        self.one_of(&[
            json!("STOP"),
            json!("STOP"),
            json!("STOP"),
            json!("MAX_TOKENS"),
            json!("MAX_TOKENS"),
            json!("SAFETY"),
            json!("max_tokens"),
            json!("stop"),
            json!(""),
            json!("MALFORMED_FUNCTION_CALL"),
            json!(5),
        ])
    }

    fn model_version(&mut self) -> Value {
        self.one_of(&[
            json!("gemini-2.5-pro"),
            json!("gemini-2.5-flash"),
            json!("gemini-3-pro-preview"),
            json!(""),
            json!(5),
        ])
    }

    fn response_id(&mut self) -> Value {
        if self.rng.chance(90) {
            json!(self.alphanumeric(12))
        } else {
            self.one_of(&[json!(""), json!(7)])
        }
    }

    fn create_time(&mut self) -> Value {
        if self.rng.chance(5) {
            return self.one_of(&[json!(5), Value::Null, json!(true)]);
        }
        json!(self.rng.pick(CREATE_TIMES))
    }

    /// One chunk as a stream line's JSON, sometimes with every non-ASCII
    /// character escaped.
    fn line(&mut self, chunk: &Value) -> String {
        let text = chunk.to_string();
        if self.rng.chance(15) {
            escape_text(&text)
        } else {
            text
        }
    }

    /// A stream as SSE `data:` lines, or bare JSON, with blank lines,
    /// comments and other events between, usually ending with `[DONE]`.
    fn data_lines(&mut self, chunks: &[Value], calls: &[String]) -> Vec<String> {
        let mut lines = Vec::new();
        for chunk in chunks {
            if self.rng.chance(5) {
                let other = self.rng.pick(&["", ": keep-alive", "event: message"]);
                lines.push(other.to_owned());
            }
            let text = self.line(chunk);
            let line = match self.rng.below(20) {
                0..=11 => format!("data: {text}"),
                12..=14 => format!("data:{text}"),
                15 => format!("data:  {text}  "),
                _ => text,
            };
            lines.push(line);
        }
        match self.rng.below(10) {
            0..=4 => lines.push("data: [DONE]".to_owned()),
            5 | 6 => lines.push("[DONE]".to_owned()),
            7 => {
                // Lines after the end still pass.
                lines.push("data: [DONE]".to_owned());
                let chunk = self.chunk(calls, 1, true, false, &None, &None);
                let text = self.line(&chunk);
                lines.push(format!("data: {text}"));
            }
            _ => {}
        }
        lines
    }

    /// A stream as upstream's executor passes it: each chunk's JSON, then
    /// usually `[DONE]`.
    fn bare_lines(&mut self, chunks: &[Value]) -> Vec<String> {
        let mut lines: Vec<String> = chunks.iter().map(|chunk| self.line(chunk)).collect();
        if self.rng.chance(4) {
            let at = self.rng.below(lines.len() + 1);
            lines.insert(at, String::new());
        }
        if self.rng.chance(75) {
            lines.push("[DONE]".to_owned());
        }
        lines
    }

    /// A whole response, or now and then something that isn't one.
    fn body(&mut self, calls: &[String]) -> String {
        match self.rng.below(50) {
            0 => String::new(),
            1 => "[]".to_owned(),
            _ => {
                let candidates = if self.rng.chance(15) { 2 } else { 1 };
                let model = self.rng.chance(85).then(|| self.model_version());
                let id = self.rng.chance(80).then(|| self.response_id());
                let response = self.chunk(calls, candidates, true, false, &model, &id);
                self.render(&response)
            }
        }
    }
}

/// `name` as upstream's `SanitizeFunctionName` makes it a Gemini function
/// name.
fn sanitize(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    let mut sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if !matches!(sanitized.as_bytes()[0], b'a'..=b'z' | b'A'..=b'Z' | b'_') {
        sanitized.truncate(63);
        sanitized.insert(0, '_');
    }
    sanitized.truncate(64);
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translator::Translator;

    #[test]
    fn cases_are_reproducible_and_valid_json() {
        for cases in [request_cases, claude_request_cases, chat_request_cases] {
            let (first, second) = (cases(7, 200), cases(7, 200));
            for (a, b) in first.iter().zip(&second) {
                assert_eq!(a.request, b.request);
                assert_eq!(a.model, b.model);
                assert_eq!(a.options, b.options);
                serde_json::from_str::<Value>(&a.request).expect("generated request is valid JSON");
            }
        }
        for cases in [event_cases, claude_event_cases, chat_event_cases] {
            let ((streams, finals), (again, finals_again)) = (cases(7, 200), cases(7, 200));
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
    fn chat_tool_call_arguments_are_json() {
        for case in chat_request_cases(3, 500) {
            let request: Value = serde_json::from_str(&case.request).expect("valid JSON");
            let calls = request["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|message| message["tool_calls"].as_array())
                .flatten();
            for call in calls {
                if let Some(arguments) = call.pointer("/function/arguments")
                    && let Some(text) = arguments.as_str()
                {
                    serde_json::from_str::<Value>(text).expect("arguments are JSON");
                }
            }
        }
    }

    #[test]
    fn sanitize_matches_upstream() {
        assert_eq!(sanitize("web search"), "web_search");
        assert_eq!(sanitize("1st_tool"), "_1st_tool");
        assert_eq!(sanitize("日本"), "__");
        assert_eq!(sanitize(&"a".repeat(70)).len(), 64);
        assert_eq!(sanitize("a.b:c-d"), "a.b:c-d");
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
        let check = |translator: Translator, outputs: &[String], needles: &[&str]| {
            for needle in needles {
                let count = outputs
                    .iter()
                    .filter(|output| output.contains(needle))
                    .count();
                assert!(
                    count >= 20,
                    "{}: {needle} in {count} outputs",
                    translator.slug()
                );
            }
        };

        let requests = outputs(Translator::GeminiGeminiRequest, &request_cases(1, 2000));
        check(
            Translator::GeminiGeminiRequest,
            &requests,
            &[
                r#""function_declarations":["#,
                r#""parametersJsonSchema":"#,
                r#""responseJsonSchema":"#,
                r#""safetySettings":[{"#,
                r#""contents":{"0":"#,
                r#""role":"model""#,
                r#""functionResponse":{"#,
            ],
        );

        let requests = outputs(
            Translator::GeminiClaudeRequest,
            &claude_request_cases(1, 2000),
        );
        check(
            Translator::GeminiClaudeRequest,
            &requests,
            &[
                r#""functionCall":"#,
                r#""functionResponse":"#,
                r#""functionDeclarations":"#,
                r#""thinkingBudget":"#,
                r#""thinkingLevel":"#,
                r#""temperature":"#,
                r#""topP":"#,
                r#""topK":"#,
                r#""systemInstruction":"#,
                r#""inline_data":"#,
                r#""functionCallingConfig":"#,
                "<system-reminder>",
            ],
        );
        let compat = outputs(
            Translator::GeminiClaudeRequestCompat,
            &claude_request_cases(1, 2000),
        );
        check(
            Translator::GeminiClaudeRequestCompat,
            &compat,
            &[r#""thought":true"#],
        );

        let requests = outputs(Translator::GeminiChatRequest, &chat_request_cases(1, 2000));
        check(
            Translator::GeminiChatRequest,
            &requests,
            &[
                r#""thinkingLevel":"#,
                r#""thinkingBudget":-1"#,
                r#""topK":"#,
                r#""maxOutputTokens":"#,
                r#""candidateCount":"#,
                r#""responseMimeType":"application/json""#,
                r#""responseJsonSchema":"#,
                r#""responseModalities":"#,
                r#""imageConfig":"#,
                r#""inlineData":"#,
                r#""thought":true"#,
                r#""functionCall":"#,
                r#""functionResponse":"#,
                r#""googleSearch":"#,
                r#""codeExecution":"#,
                r#""urlContext":"#,
                r#""mode":"VALIDATED""#,
                r#""mode":"ANY""#,
                r#""allowedFunctionNames":"#,
                r#""mode":"NONE""#,
                r#""systemInstruction":"#,
                r#""thoughtSignature":"#,
            ],
        );

        let (streams, finals) = event_cases(1, 2000);
        let streams = outputs(Translator::GeminiGeminiStream, &streams);
        check(
            Translator::GeminiGeminiStream,
            &streams,
            &[r#"\"usageMetadata\""#, r#""""#, "event: message"],
        );
        let finals = outputs(Translator::GeminiGeminiNonStream, &finals);
        check(Translator::GeminiGeminiNonStream, &finals, &["candidates"]);

        let (streams, finals) = claude_event_cases(1, 2000);
        let streams = outputs(Translator::GeminiClaudeStream, &streams);
        check(
            Translator::GeminiClaudeStream,
            &streams,
            &[
                r#""type":"tool_use""#,
                r#""type":"thinking_delta""#,
                r#""type":"signature_delta""#,
                r#""type":"input_json_delta""#,
                r#""type":"text_delta""#,
                r#""stop_reason":"tool_use""#,
                r#""stop_reason":"max_tokens""#,
                r#""stop_reason":"end_turn""#,
                r#""cache_read_input_tokens":"#,
                r#""event":"message_stop""#,
                r#""name":"web search""#,
                r#""name":"Bash""#,
            ],
        );
        let finals = outputs(Translator::GeminiClaudeNonStream, &finals);
        check(
            Translator::GeminiClaudeNonStream,
            &finals,
            &[
                r#""type":"tool_use""#,
                r#""type":"thinking""#,
                r#""signature":"#,
                r#""stop_reason":"max_tokens""#,
                r#""cache_read_input_tokens":"#,
            ],
        );

        let (streams, finals) = chat_event_cases(1, 2000);
        let streams = outputs(Translator::GeminiChatStream, &streams);
        check(
            Translator::GeminiChatStream,
            &streams,
            &[
                r#""reasoning_content":""#,
                r#""tool_calls":["#,
                r#""images":["#,
                r#""finish_reason":"tool_calls""#,
                r#""finish_reason":"max_tokens""#,
                r#""finish_reason":"stop""#,
                r#""reasoning_tokens":"#,
                r#""cached_tokens":"#,
                r#""name":"web search""#,
                r#""created":17"#,
                r#""index":1"#,
            ],
        );
        let finals = outputs(Translator::GeminiChatNonStream, &finals);
        check(
            Translator::GeminiChatNonStream,
            &finals,
            &[
                r#""tool_calls":["#,
                r#""images":["#,
                r#""reasoning_content":""#,
                r#""finish_reason":"tool_calls""#,
                r#""finish_reason":"max_tokens""#,
                r#""reasoning_tokens":"#,
            ],
        );
    }
}
