//! Seeded random input for the Chat Completions translators.
//!
//! Requests mix message roles, content parts and tool calls with loosely typed
//! values. They declare function, custom and built-in tools whose names need
//! sanitizing or shortening, or collide once shortened. Tool messages answer
//! calls by ID, by an ID shared with another call, or by none.
//!
//! Event streams come from the Codex stream generator ([`super::response`]),
//! with the request replaced by a Chat request declaring the same tools. To
//! them are added the events only this translator reads: `apply_patch` calls
//! streamed as custom tool input, generated images, reasoning text, service
//! tiers, creation times and models.

use std::ops::{Deref, DerefMut};

use serde_json::{Value, json};

use super::{EFFORTS, Rng, SERVICE_TIERS, escape_text, num, to_object};
use crate::cases::Case;

/// Builds `count` random Chat Completions requests.
pub fn request_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed, index);
            let model = generator.model();
            let request = generator.request();
            let text = generator.render(&request);
            Case::new(format!("random-{seed}-{index}"), model, text)
        })
        .collect()
}

/// Builds `count` random Codex event streams, and a non-streaming case from
/// the final event of each.
pub fn event_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    let mixed = seed.rotate_left(17);
    let (streams, finals) = super::response::cases(mixed, count);
    streams
        .into_iter()
        .zip(finals)
        .enumerate()
        .map(|(index, (stream, last))| {
            let mut generator = Generator::new(mixed, index as u64);
            let model = generator.rng.pick(&["gpt-5", "gpt-5.6-luna", "", " "]);
            let claude: Value = serde_json::from_str(&stream.request).unwrap_or_default();
            let request = generator.original_request(&claude);
            let request = generator.render(&request);
            let items = generator.extra_items();
            let lines = generator.event_lines(stream.events, &items);
            let body = generator.final_body(&last.events[0], &items);
            let case = |events| Case {
                events,
                ..Case::new(format!("random-{seed}-{index}"), model, request.clone())
            };
            (case(lines), case(vec![body]))
        })
        .unzip()
}

const SERVICE_TIERS_CODEX: &[&str] = &[
    "ultrafast",
    "ULTRAFAST",
    " UltraFast ",
    "ultra fast",
    "ultrafast\u{a0}",
];

/// Patch text, as the custom `apply_patch` tool takes it.
const PATCHES: &[&str] = &[
    "*** Begin Patch\n*** Add File: hello.txt\n+Hello, world!\n*** End Patch\n",
    "*** Begin Patch\n*** Update File: src/main.rs\n@@\n-    let x = \"old\";\n+    let x = \"new <&>\";\n*** End Patch",
    "",
    " ",
    "line\r\n\ttab \\ backslash",
    "补丁 🙂 \u{2028}",
    "{\"input\":\"nested\"}",
];

/// Function call arguments, including `{"input": …}` envelopes that are and
/// aren't exactly one string field.
const ARGUMENTS: &[&str] = &[
    "{\"city\":\"Paris\"}",
    "{\"city\": \"Zürich\", \"unit\": \"celsius\"}",
    "{}",
    "",
    "not json",
    "{\"input\":\"*** Begin Patch\\n*** End Patch\\n\"}",
    "{\"input\":\"line\\r\\n\\u003c\\u0026\\u003e \\ud83d\\ude42\"}",
    " { \"input\" : \"padded\" } ",
    "{\"input\":5}",
    "{\"input\":\"a\",\"other\":1}",
    "{\"input\":\"a\"} trailing",
    "{\"\\u0069nput\":\"escaped key\"}",
];

const IMAGES: &[&str] = &[
    "iVBORw0KGgoAAAANSUhEUg==",
    "iVBORw0KGgoAAAANSUhEUg==",
    "/9j/4AAQSkZJRg==",
    "UklGRiQAAABXRUJQ",
    "",
];

const IMAGE_FORMATS: &[&str] = &["png", "jpeg", "JPG", "webp", "gif", "image/avif", "bmp", ""];

const NAMESPACES: &[&str] = &["functions", "ns", "tools__", "mcp__srv"];

/// The Claude request generator, for its leaf values: text, numbers, tool
/// names and schemas.
struct Generator {
    base: super::Generator,
    /// The namespace the request declared `apply_patch` in, if any.
    namespace: Option<String>,
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

/// An output item only the Chat Completions translator reads: the events
/// that stream it, and its final form.
struct Item {
    events: Vec<Value>,
    done: Value,
}

impl Generator {
    fn new(seed: u64, index: u64) -> Self {
        Self {
            base: super::Generator {
                // A different mix from the other generators', so cases don't
                // share their random choices.
                rng: Rng(seed.rotate_left(48) ^ index.wrapping_mul(0xC2B2_AE3D_27D4_EB4F)),
                tool_names: Vec::new(),
                tool_use_ids: Vec::new(),
            },
            namespace: None,
        }
    }

    // --- Requests ---

    fn request(&mut self) -> Value {
        let mut fields = Vec::new();
        // Tools come first so messages and tool_choice can use their names.
        if self.rng.chance(60) {
            fields.push(("tools", self.tools()));
        }
        if self.rng.chance(80) {
            fields.push(("model", self.model().into()));
        }
        if self.rng.chance(95) {
            fields.push(("messages", self.messages()));
        }
        if self.rng.chance(35) {
            fields.push(("tool_choice", self.tool_choice()));
        }
        if self.rng.chance(40) {
            fields.push(("reasoning_effort", self.effort()));
        }
        if self.rng.chance(30) {
            fields.push(("service_tier", self.service_tier()));
        }
        if self.rng.chance(20) {
            fields.push(("response_format", self.response_format()));
        }
        if self.rng.chance(15) {
            fields.push(("text", self.text_options()));
        }
        let extras = [
            ("stream", json!(true)),
            ("stream_options", json!({ "include_usage": true })),
            ("temperature", num("0.7")),
            ("max_tokens", json!(1024)),
            ("parallel_tool_calls", json!(false)),
            ("store", json!(true)),
            ("user", json!("user_123")),
        ];
        for (key, value) in extras {
            if self.rng.chance(10) {
                fields.push((key, value));
            }
        }
        self.rng.shuffle(&mut fields);
        to_object(fields)
    }

    fn tools(&mut self) -> Value {
        if self.rng.chance(4) {
            return self.one_of(&[json!({}), json!("tools"), Value::Null, json!([])]);
        }
        let count = 1 + self.rng.below(5);
        Value::Array((0..count).map(|_| self.tool()).collect())
    }

    fn tool(&mut self) -> Value {
        match self.rng.below(100) {
            0..=59 => self.function_tool(),
            60..=74 => self.custom_tool(),
            75..=82 => {
                // apply_patch as Codex declares it, or as a function.
                self.tool_names.push(json!("apply_patch"));
                if self.rng.chance(70) {
                    json!({ "type": "custom", "name": "apply_patch", "description": "Apply a patch.", "format": { "type": "grammar", "syntax": "lark", "definition": "start: begin_patch" } })
                } else {
                    json!({ "type": "function", "function": { "name": "apply_patch", "parameters": { "type": "object", "properties": { "input": { "type": "string" } } } } })
                }
            }
            83..=91 => self.one_of(&[
                json!({ "type": "web_search" }),
                json!({ "type": "web_search_preview", "search_context_size": "low" }),
                json!({ "type": "image_generation", "output_format": "png" }),
                json!({ "type": "code_interpreter", "container": { "type": "auto" } }),
                json!({ "type": "file_search", "vector_store_ids": ["vs_1"] }),
            ]),
            _ => self.one_of(&[
                json!(5),
                json!("tool"),
                Value::Null,
                json!([]),
                json!({}),
                json!({ "type": "" }),
                json!({ "type": 5 }),
                json!({ "function": { "name": "untyped" } }),
                json!({ "type": "Function", "function": { "name": "mixed_case" } }),
            ]),
        }
    }

    fn function_tool(&mut self) -> Value {
        let mut function = Vec::new();
        if self.rng.chance(92) {
            let name = self.tool_name();
            self.tool_names.push(name.clone());
            function.push(("name", name));
        }
        if self.rng.chance(60) {
            function.push(("description", self.loose_text()));
        }
        if self.rng.chance(85) {
            function.push(("parameters", self.schema(0)));
        }
        if self.rng.chance(30) {
            function.push(("strict", self.bool_like()));
        }
        let function = if self.rng.chance(96) {
            self.object(function)
        } else {
            self.one_of(&[Value::Null, json!("f"), json!([])])
        };
        self.object(vec![("type", json!("function")), ("function", function)])
    }

    fn custom_tool(&mut self) -> Value {
        let mut fields = vec![("type", json!("custom"))];
        if self.rng.chance(92) {
            let name = self.tool_name();
            self.tool_names.push(name.clone());
            fields.push(("name", name));
        }
        if self.rng.chance(50) {
            fields.push(("description", self.text().into()));
        }
        if self.rng.chance(50) {
            let format = self.one_of(&[
                json!({ "type": "text" }),
                json!({ "type": "grammar", "syntax": "regex", "definition": "^\\d+$" }),
            ]);
            fields.push(("format", format));
        }
        self.object(fields)
    }

    fn messages(&mut self) -> Value {
        if self.rng.chance(3) {
            return self.one_of(&[
                json!("hi"),
                Value::Null,
                json!({ "role": "user" }),
                json!(5),
            ]);
        }
        let count = self.rng.below(8);
        let mut messages = Vec::new();
        while messages.len() < count {
            match self.rng.below(100) {
                0..=11 => {
                    let role = self.rng.pick(&["system", "system", "developer"]);
                    messages.push(self.message(role));
                }
                12..=41 => messages.push(self.message("user")),
                42..=71 => {
                    messages.push(self.assistant_message());
                    // Tool messages usually answer the calls, in any order.
                    if self.rng.chance(75) {
                        let mut ids = self.tool_use_ids.clone();
                        self.rng.shuffle(&mut ids);
                        if self.rng.chance(20) {
                            ids.truncate(self.rng.below(ids.len() + 1));
                        }
                        for id in ids {
                            messages.push(self.tool_message(Some(id)));
                        }
                    }
                }
                72..=89 => {
                    let id = self.answer_id();
                    messages.push(self.tool_message(id));
                }
                _ => messages.push(self.odd_message()),
            }
        }
        Value::Array(messages)
    }

    fn message(&mut self, role: &str) -> Value {
        let mut fields = vec![("role", json!(role))];
        if let Some(content) = self.content() {
            fields.push(("content", content));
        }
        if self.rng.chance(10) {
            fields.push(("name", json!("alice")));
        }
        self.object(fields)
    }

    fn odd_message(&mut self) -> Value {
        self.one_of(&[
            json!("hi"),
            json!(5),
            Value::Null,
            json!({ "content": "no role" }),
            json!({ "role": "function", "name": "f", "content": "result" }),
            json!({ "role": "System", "content": "mixed case" }),
            json!({ "role": 5, "content": "numeric role" }),
            json!({ "role": "assistant", "content": [], "tool_calls": [] }),
            json!({ "role": "user", "content": [{ "type": "text", "text": "" }] }),
        ])
    }

    fn content(&mut self) -> Option<Value> {
        Some(match self.rng.below(100) {
            0..=44 => self.text().into(),
            45..=79 => {
                let count = self.rng.below(4);
                Value::Array((0..count).map(|_| self.content_part()).collect())
            }
            80..=84 => json!(""),
            85..=89 => Value::Null,
            90..=94 => return None,
            _ => self.one_of(&[json!(5), json!({ "text": "object" }), json!(true)]),
        })
    }

    fn content_part(&mut self) -> Value {
        match self.rng.below(100) {
            0..=44 => {
                let mut fields = vec![("type", json!("text"))];
                if self.rng.chance(92) {
                    fields.push(("text", self.loose_text()));
                }
                to_object(fields)
            }
            45..=59 => {
                let image_url = match self.rng.below(6) {
                    0 => json!("https://example.com/cat.png"),
                    1 => json!({}),
                    _ => json!({ "url": self.image_url(), "detail": "high" }),
                };
                json!({ "type": "image_url", "image_url": image_url })
            }
            60..=69 => {
                let mut file = Vec::new();
                if self.rng.chance(80) {
                    let data =
                        self.rng
                            .pick(&["data:application/pdf;base64,JVBERi0=", "", "JVBERi0="]);
                    file.push(("file_data", json!(data)));
                }
                if self.rng.chance(60) {
                    file.push((
                        "filename",
                        json!(self.rng.pick(&["a.pdf", "", "résumé.pdf"])),
                    ));
                }
                if self.rng.chance(20) {
                    file.push(("file_id", json!("file_123")));
                }
                json!({ "type": "file", "file": to_object(file) })
            }
            70..=79 => {
                let mut audio = Vec::new();
                if self.rng.chance(85) {
                    audio.push(("data", json!(self.rng.pick(&["UklGRg==", ""]))));
                }
                if self.rng.chance(70) {
                    audio.push(("format", json!(self.rng.pick(&["wav", "mp3", ""]))));
                }
                json!({ "type": "input_audio", "input_audio": to_object(audio) })
            }
            80..=84 => json!({ "type": "refusal", "refusal": "No." }),
            _ => self.one_of(&[
                json!("plain"),
                json!(5),
                Value::Null,
                json!({ "text": "no type" }),
                json!({ "type": "Text", "text": "mixed case" }),
                json!({ "type": "input_text", "text": "Responses part" }),
            ]),
        }
    }

    fn image_url(&mut self) -> Value {
        self.one_of(&[
            json!("https://example.com/cat.png"),
            json!("data:image/png;base64,iVBORw0KGgo="),
            json!(""),
            json!(" "),
            json!(5),
        ])
    }

    /// An assistant message, with tool calls whose IDs it keeps for the tool
    /// messages that follow. A missing ID is kept as null.
    fn assistant_message(&mut self) -> Value {
        self.tool_use_ids.clear();
        let mut fields = vec![("role", json!("assistant"))];
        if self.rng.chance(70)
            && let Some(content) = self.content()
        {
            fields.push(("content", content));
        }
        if self.rng.chance(60) {
            let calls = if self.rng.chance(5) {
                self.one_of(&[json!("calls"), json!({}), Value::Null])
            } else {
                let count = 1 + self.rng.below(4);
                Value::Array((0..count).map(|_| self.tool_call()).collect())
            };
            fields.push(("tool_calls", calls));
        }
        self.object(fields)
    }

    fn tool_call(&mut self) -> Value {
        let id = self.call_id();
        self.tool_use_ids.push(id.clone().unwrap_or_default());
        let name = self.call_name();
        let mut fields = Vec::new();
        if let Some(id) = id {
            fields.push(("id", id));
        }
        match self.rng.below(100) {
            0..=69 => {
                fields.push(("type", json!("function")));
                let function = self.function_call(name);
                fields.push(("function", function));
            }
            70..=84 => {
                fields.push(("type", json!("custom")));
                let mut custom = Vec::new();
                if let Some(name) = name {
                    custom.push(("name", name));
                }
                if self.rng.chance(92) {
                    custom.push(("input", json!(self.rng.pick(PATCHES))));
                }
                fields.push(("custom", self.object(custom)));
            }
            85..=92 => {
                if self.rng.chance(50) {
                    let kind = self.one_of(&[json!("Function"), json!(5), Value::Null]);
                    fields.push(("type", kind));
                }
                let function = self.function_call(name);
                fields.push(("function", function));
            }
            _ => return self.one_of(&[json!("call"), json!(5), Value::Null]),
        }
        self.object(fields)
    }

    fn function_call(&mut self, name: Option<Value>) -> Value {
        let mut function = Vec::new();
        if let Some(name) = name {
            function.push(("name", name));
        }
        if self.rng.chance(92) {
            let arguments = match self.rng.below(100) {
                0..=79 => json!(self.rng.pick(ARGUMENTS)),
                80..=89 => json!({ "city": "Paris", "n": num("1.50") }),
                _ => self.one_of(&[json!(5), Value::Null, json!(true)]),
            };
            function.push(("arguments", arguments));
        }
        self.object(function)
    }

    fn call_id(&mut self) -> Option<Value> {
        let earlier: Vec<Value> = self
            .tool_use_ids
            .iter()
            .filter(|id| !id.is_null())
            .cloned()
            .collect();
        Some(match self.rng.below(100) {
            0..=9 => return None,
            10..=14 => json!(""),
            // Shared with an earlier call in the same message.
            15..=24 if !earlier.is_empty() => self.rng.pick(&earlier),
            25..=27 => json!(7),
            // The ID generated for a call without one.
            28..=29 => json!(self.rng.pick(&["call_missing_1_0", "call_missing_2_1"])),
            _ => json!(format!("call_{}", self.alphanumeric(8))),
        })
    }

    fn call_name(&mut self) -> Option<Value> {
        let names = self.tool_names.clone();
        Some(match self.rng.below(100) {
            0..=2 => return None,
            3..=62 if !names.is_empty() => self.rng.pick(&names),
            63..=77 => json!("apply_patch"),
            _ => self.tool_name(),
        })
    }

    /// The `tool_call_id` of a tool message that doesn't follow its call.
    fn answer_id(&mut self) -> Option<Value> {
        let ids = self.tool_use_ids.clone();
        Some(match self.rng.below(100) {
            0..=49 if !ids.is_empty() => self.rng.pick(&ids),
            50..=59 => json!(""),
            60..=69 => return None,
            70..=79 => json!(5),
            _ => json!("call_unknown"),
        })
    }

    fn tool_message(&mut self, id: Option<Value>) -> Value {
        let mut fields = vec![("role", json!("tool"))];
        match id {
            Some(Value::Null) if self.rng.chance(50) => {}
            Some(id) => fields.push(("tool_call_id", id)),
            None => {}
        }
        if let Some(content) = self.tool_content() {
            fields.push(("content", content));
        }
        self.object(fields)
    }

    fn tool_content(&mut self) -> Option<Value> {
        Some(match self.rng.below(100) {
            0..=34 => self.text().into(),
            35..=49 => {
                let count = self.rng.below(4);
                Value::Array((0..count).map(|_| self.tool_output_part()).collect())
            }
            50..=64 => {
                // Parts written as a string, which are read as parts if one is
                // an image.
                let count = 1 + self.rng.below(3);
                let parts = Value::Array((0..count).map(|_| self.tool_output_part()).collect());
                let text = if self.rng.chance(20) {
                    format!(" \n{parts}")
                } else {
                    parts.to_string()
                };
                text.into()
            }
            65..=69 => self.one_of(&[
                json!("[]"),
                json!("[1,2]"),
                json!("[\"a\"]"),
                json!("[not json"),
            ]),
            70..=79 => self.one_of(&[
                json!({ "result": num("1.50"), "ok": true }),
                json!([]),
                num("1e3"),
                json!(true),
            ]),
            80..=84 => Value::Null,
            85..=89 => return None,
            _ => json!(""),
        })
    }

    fn tool_output_part(&mut self) -> Value {
        match self.rng.below(100) {
            0..=29 => {
                let kind = self.rng.pick(&["text", "input_text", "output_text"]);
                let mut fields = vec![("type", json!(kind))];
                if self.rng.chance(90) {
                    fields.push(("text", self.loose_text()));
                }
                to_object(fields)
            }
            30..=44 => {
                let mut image = Vec::new();
                if self.rng.chance(80) {
                    image.push(("url", self.image_url()));
                }
                if self.rng.chance(20) {
                    image.push(("file_id", json!("file_img")));
                }
                if self.rng.chance(30) {
                    image.push(("detail", json!(self.rng.pick(&["low", "high", "auto", ""]))));
                }
                json!({ "type": "image_url", "image_url": to_object(image) })
            }
            45..=54 => {
                let mut fields = vec![("type", json!("input_image"))];
                if self.rng.chance(70) {
                    fields.push(("image_url", self.image_url()));
                }
                if self.rng.chance(30) {
                    fields.push(("file_id", json!("file_img")));
                }
                if self.rng.chance(30) {
                    fields.push(("detail", json!("low")));
                }
                to_object(fields)
            }
            55..=69 => {
                let mut file = Vec::new();
                for (key, value) in [
                    ("file_id", "file_1"),
                    ("file_data", "JVBERi0="),
                    ("file_url", "https://example.com/a.pdf"),
                    ("filename", "a.pdf"),
                ] {
                    if self.rng.chance(35) {
                        file.push((key, json!(value)));
                    }
                }
                json!({ "type": "file", "file": to_object(file) })
            }
            _ => self.one_of(&[
                json!("plain"),
                json!(5),
                Value::Null,
                json!({ "type": "audio", "data": "UklGRg==" }),
                json!({ "text": "no type" }),
                json!({ "type": "refusal", "refusal": "No." }),
            ]),
        }
    }

    fn tool_choice(&mut self) -> Value {
        match self.rng.below(100) {
            0..=29 => self.loose_choice(&["auto", "none", "required", "Auto", ""]),
            30..=54 => {
                let name = self.choice_name();
                json!({ "type": "function", "function": { "name": name } })
            }
            55..=59 => json!({ "type": "function", "name": self.choice_name() }),
            60..=69 => json!({ "type": "custom", "name": self.choice_name() }),
            70..=74 => json!({ "type": "custom" }),
            75..=84 => self.one_of(&[
                json!({ "type": "allowed_tools", "mode": "auto", "tools": [{ "type": "function", "name": "get_weather" }] }),
                json!({ "type": "web_search" }),
                json!({ "type": "image_generation" }),
            ]),
            _ => self.one_of(&[
                json!({}),
                json!({ "type": "" }),
                json!({ "type": 5, "name": "x" }),
                json!(["auto"]),
                json!(5),
                Value::Null,
            ]),
        }
    }

    fn choice_name(&mut self) -> Value {
        let names = self.tool_names.clone();
        if !names.is_empty() && self.rng.chance(70) {
            self.rng.pick(&names)
        } else {
            self.tool_name()
        }
    }

    fn effort(&mut self) -> Value {
        match self.rng.below(10) {
            0 => self.number(),
            1 => self.one_of(&[
                Value::Null,
                json!(true),
                json!({ "b": 1, "a": [num("1.50")] }),
                json!(["high"]),
            ]),
            _ => self.loose_choice(EFFORTS),
        }
    }

    fn service_tier(&mut self) -> Value {
        match self.rng.below(100) {
            0..=59 => self.rng.pick(SERVICE_TIERS).into(),
            60..=84 => self.rng.pick(SERVICE_TIERS_CODEX).into(),
            _ => self.one_of(&[json!(5), Value::Null, json!(true), json!({})]),
        }
    }

    fn response_format(&mut self) -> Value {
        match self.rng.below(100) {
            0..=19 => json!({ "type": "text" }),
            20..=29 => json!({ "type": "json_object" }),
            30..=79 => {
                let mut schema = Vec::new();
                if self.rng.chance(80) {
                    schema.push(("name", self.loose_choice(&["answer", "", "résumé"])));
                }
                if self.rng.chance(50) {
                    schema.push(("strict", self.bool_like()));
                }
                if self.rng.chance(85) {
                    schema.push(("schema", self.schema(0)));
                }
                if self.rng.chance(10) {
                    schema.push(("description", json!("not copied")));
                }
                json!({ "type": "json_schema", "json_schema": self.object(schema) })
            }
            80..=84 => self.one_of(&[
                json!({ "type": "json_schema" }),
                json!({ "type": "json_schema", "json_schema": "answer" }),
            ]),
            _ => self.one_of(&[
                json!("json"),
                Value::Null,
                json!(5),
                json!({ "type": 5 }),
                json!({}),
            ]),
        }
    }

    fn text_options(&mut self) -> Value {
        match self.rng.below(10) {
            0 => json!({}),
            1 => self.one_of(&[json!("low"), Value::Null, json!(5)]),
            _ => json!({ "verbosity": self.loose_choice(&["low", "medium", "high", ""]) }),
        }
    }

    // --- Event streams ---

    /// A Chat request declaring the Claude request's tools as functions, and
    /// usually `apply_patch` as a custom tool: at the top level, also as a
    /// function, or in a namespace.
    fn original_request(&mut self, claude: &Value) -> Value {
        self.namespace = None;
        let declared = claude.get("tools").and_then(Value::as_array);
        let mut tools: Vec<Value> = declared
            .into_iter()
            .flatten()
            .map(|tool| match tool.get("name") {
                Some(name) if self.rng.chance(90) => {
                    json!({ "type": "function", "function": { "name": name, "parameters": { "type": "object" } } })
                }
                _ => tool.clone(),
            })
            .collect();
        let patch = json!({ "type": "custom", "name": "apply_patch" });
        let mut fields = Vec::new();
        match self.rng.below(100) {
            0..=39 => tools.push(patch),
            40..=49 => {
                tools.push(patch);
                tools.push(json!({ "type": "function", "function": { "name": "apply_patch" } }));
            }
            50..=59 => {
                let namespace = self.rng.pick(NAMESPACES);
                tools.push(json!({ "type": "namespace", "name": namespace, "tools": [patch] }));
                self.namespace = Some(namespace.to_owned());
            }
            60..=69 => {
                let namespace = self.rng.pick(NAMESPACES);
                let tool = json!({ "type": "namespace", "name": namespace, "tools": [patch] });
                fields.push((
                    "input",
                    json!([{ "type": "additional_tools", "tools": [tool] }]),
                ));
                self.namespace = Some(namespace.to_owned());
            }
            _ => {}
        }
        self.rng.shuffle(&mut tools);
        match self.rng.below(100) {
            // gjson reads a single tool, not in a list, as a list of one.
            0..=3 => fields.push(("tools", tools.into_iter().next().unwrap_or_default())),
            4..=7 => {}
            _ => fields.push(("tools", Value::Array(tools))),
        }
        fields.push(("messages", json!([{ "role": "user", "content": "hi" }])));
        self.rng.shuffle(&mut fields);
        to_object(fields)
    }

    fn extra_items(&mut self) -> Vec<Item> {
        let count = self.rng.below(4);
        (0..count)
            .map(|n| {
                // After the stream's own items, or sharing an index with one.
                let index = match self.rng.below(100) {
                    0..=79 => Some(json!(6 + n)),
                    80..=89 => Some(json!(self.rng.below(3))),
                    90..=94 => Some(json!((6 + n).to_string())),
                    _ => None,
                };
                match self.rng.below(100) {
                    0..=49 => self.custom_call(index),
                    50..=79 => self.image(index),
                    _ => self.reasoning_text(index),
                }
            })
            .collect()
    }

    fn custom_call(&mut self, index: Option<Value>) -> Item {
        let id = self.item_id("ctc");
        let call_id = format!("call_{}", self.alphanumeric(8));
        let (namespace, name) = self.custom_name();
        let input = self.rng.pick(PATCHES);
        let item = |input: &str, status: &str| {
            let mut fields = vec![("type", json!("custom_tool_call"))];
            if let Some(id) = &id {
                fields.push(("id", json!(id)));
            }
            fields.push(("call_id", json!(call_id)));
            if let Some(namespace) = &namespace {
                fields.push(("namespace", json!(namespace)));
            }
            fields.push(("name", json!(name)));
            fields.push(("input", json!(input)));
            fields.push(("status", json!(status)));
            to_object(fields)
        };
        let keys = |generator: &mut Self| match &id {
            Some(id) if generator.rng.chance(85) => vec![("item_id", json!(id))],
            _ => Vec::new(),
        };

        let mut events = Vec::new();
        if self.rng.chance(85) {
            let added = item("", "in_progress");
            events.push(event(
                "response.output_item.added",
                &index,
                vec![("item", added)],
            ));
        }
        if self.rng.chance(80) {
            for chunk in self.chunks(input) {
                let mut fields = keys(self);
                fields.push(("delta", json!(chunk)));
                events.push(event(
                    "response.custom_tool_call_input.delta",
                    &index,
                    fields,
                ));
            }
        }
        if self.rng.chance(70) {
            let mut fields = keys(self);
            fields.push(("input", json!(input)));
            events.push(event(
                "response.custom_tool_call_input.done",
                &index,
                fields,
            ));
        }
        let done = item(input, "completed");
        events.push(event(
            "response.output_item.done",
            &index,
            vec![("item", done.clone())],
        ));
        Item {
            events: self.jumble(events),
            done,
        }
    }

    /// A custom tool call's namespace and name: usually `apply_patch`, in the
    /// namespace the request declared it in or another.
    fn custom_name(&mut self) -> (Option<String>, String) {
        match self.rng.below(100) {
            0..=59 => {
                let namespace = self.namespace.clone().filter(|_| self.rng.chance(80));
                (namespace, "apply_patch".into())
            }
            60..=69 => (None, " apply_patch ".into()),
            70..=79 => (Some("other".into()), "apply_patch".into()),
            _ => (
                None,
                self.rng.pick(&["shell", "get_weather", "Bash"]).into(),
            ),
        }
    }

    fn image(&mut self, index: Option<Value>) -> Item {
        let id = self.item_id("ig");
        let format = self.rng.chance(80).then(|| self.rng.pick(IMAGE_FORMATS));
        let item = |status: &str, result: Option<&str>| {
            let mut fields = vec![("type", json!("image_generation_call"))];
            if let Some(id) = &id {
                fields.push(("id", json!(id)));
            }
            fields.push(("status", json!(status)));
            if let Some(result) = result {
                fields.push(("result", json!(result)));
            }
            if let Some(format) = format {
                fields.push(("output_format", json!(format)));
            }
            to_object(fields)
        };

        let added = item("in_progress", None);
        let mut events = vec![event(
            "response.output_item.added",
            &index,
            vec![("item", added)],
        )];
        // Partial images, sometimes the same one again.
        let mut last = None;
        for partial_index in 0..self.rng.below(4) {
            let b64 = self.rng.pick(IMAGES);
            let mut fields = Vec::new();
            if let Some(id) = &id
                && self.rng.chance(90)
            {
                fields.push(("item_id", json!(id)));
            }
            fields.push(("partial_image_index", json!(partial_index)));
            fields.push(("partial_image_b64", json!(b64)));
            if let Some(format) = format {
                fields.push(("output_format", json!(format)));
            }
            events.push(event(
                "response.image_generation_call.partial_image",
                &index,
                fields,
            ));
            last = Some(b64);
        }
        let result = match self.rng.below(10) {
            0 => None,
            1..=5 if last.is_some() => last,
            _ => Some(self.rng.pick(IMAGES)),
        };
        let done = item("completed", result);
        events.push(event(
            "response.output_item.done",
            &index,
            vec![("item", done.clone())],
        ));
        Item {
            events: self.jumble(events),
            done,
        }
    }

    fn reasoning_text(&mut self, index: Option<Value>) -> Item {
        let id = format!("rs_{}", self.alphanumeric(8));
        let text = self.text();
        let at = || vec![("item_id", json!(id)), ("content_index", json!(0))];

        let added = json!({ "type": "reasoning", "id": id, "summary": [] });
        let mut events = vec![event(
            "response.output_item.added",
            &index,
            vec![("item", added)],
        )];
        for chunk in self.chunks(&text) {
            let mut fields = at();
            if self.rng.chance(95) {
                fields.push(("delta", json!(chunk)));
            }
            events.push(event("response.reasoning_text.delta", &index, fields));
        }
        let mut fields = at();
        fields.push(("text", json!(text)));
        events.push(event("response.reasoning_text.done", &index, fields));
        let done = json!({ "type": "reasoning", "id": id, "summary": [], "content": [{ "type": "reasoning_text", "text": text }] });
        events.push(event(
            "response.output_item.done",
            &index,
            vec![("item", done.clone())],
        ));
        Item {
            events: self.jumble(events),
            done,
        }
    }

    fn item_id(&mut self, prefix: &str) -> Option<String> {
        match self.rng.below(100) {
            0..=4 => None,
            5..=7 => Some(String::new()),
            _ => Some(format!("{prefix}_{}", self.alphanumeric(8))),
        }
    }

    /// The stream's lines with some events varied, and the extra items' events
    /// added before the final event.
    fn event_lines(&mut self, lines: Vec<String>, items: &[Item]) -> Vec<String> {
        let mut lines: Vec<String> = lines.into_iter().map(|line| self.vary_line(line)).collect();
        let is_final = |line: &String| {
            [
                "\"response.completed\"",
                "\"response.incomplete\"",
                "\"response.failed\"",
            ]
            .iter()
            .any(|kind| line.contains(kind))
        };
        let mut end = lines.iter().rposition(is_final).unwrap_or(lines.len());
        for item in items {
            let at = self.rng.below(end + 1);
            let item_lines: Vec<String> = item
                .events
                .iter()
                .map(|event| self.data_line(event))
                .collect();
            end += item_lines.len();
            lines.splice(at..at, item_lines);
        }
        lines
    }

    fn vary_line(&mut self, line: String) -> String {
        let event = line
            .strip_prefix("data:")
            .and_then(|data| serde_json::from_str::<Value>(data.trim()).ok());
        match event {
            Some(mut event @ Value::Object(_)) => {
                if self.vary_event(&mut event) {
                    self.data_line(&event)
                } else {
                    line
                }
            }
            _ => line,
        }
    }

    /// Adds creation times, service tiers and models to an event. Returns
    /// whether anything changed.
    fn vary_event(&mut self, event: &mut Value) -> bool {
        let mut changed = false;
        let kind = event["type"].as_str().unwrap_or_default().to_owned();
        let start = kind == "response.created";
        let end = matches!(kind.as_str(), "response.completed" | "response.incomplete");
        if (start || end)
            && let Some(response @ Value::Object(_)) = event.get_mut("response")
        {
            if self.rng.chance(if start { 85 } else { 10 }) {
                response["created_at"] = self.created_at();
                changed = true;
            }
            if self.rng.chance(30) {
                response["service_tier"] = self.codex_tier();
                changed = true;
            }
            if start && self.rng.chance(15) {
                response["model"] = self.model_value();
                changed = true;
            }
        }
        if self.rng.chance(4) {
            event["model"] = self.model_value();
            changed = true;
        }
        if self.rng.chance(4) {
            event["service_tier"] = self.codex_tier();
            changed = true;
        }
        changed
    }

    /// The final event with the extra items in its output, and a creation time
    /// and service tier added.
    fn final_body(&mut self, last: &str, items: &[Item]) -> String {
        let Ok(mut event) = serde_json::from_str::<Value>(last) else {
            return last.to_owned();
        };
        if let Some(Value::Array(output)) = event.pointer_mut("/response/output") {
            for item in items {
                if self.rng.chance(85) {
                    let at = self.rng.below(output.len() + 1);
                    output.insert(at, item.done.clone());
                }
            }
        }
        if let Some(response @ Value::Object(_)) = event.get_mut("response") {
            // Without one, upstream and we each use the current time.
            if self.rng.chance(85) {
                response["created_at"] = self.created_at();
            }
            if self.rng.chance(30) {
                response["service_tier"] = self.codex_tier();
            }
        }
        if event.is_object() && self.rng.chance(5) {
            event["service_tier"] = self.codex_tier();
        }
        self.render(&event)
    }

    fn created_at(&mut self) -> Value {
        self.one_of(&[
            json!(1_700_000_000),
            json!(1_700_000_000),
            json!("1700000000"),
            num("1.7e9"),
            num("1700000000.9"),
            json!("soon"),
            Value::Null,
            json!(-1),
            num("99999999999999999999"),
            json!(true),
        ])
    }

    fn codex_tier(&mut self) -> Value {
        self.one_of(&[
            json!("priority"),
            json!("flex"),
            json!(" default "),
            json!(""),
            json!("ultrafast"),
            json!(5),
            Value::Null,
        ])
    }

    fn model_value(&mut self) -> Value {
        self.one_of(&[
            json!("gpt-5-codex"),
            json!(""),
            json!(" "),
            Value::Null,
            json!(5),
            json!({ "id": "gpt-5" }),
        ])
    }

    fn data_line(&mut self, event: &Value) -> String {
        let mut text = event.to_string();
        if self.rng.chance(5) {
            text = escape_text(&text);
        }
        let prefix = if self.rng.chance(10) {
            "data:"
        } else {
            "data: "
        };
        format!("{prefix}{text}")
    }

    /// Drops or repeats a few events.
    fn jumble(&mut self, events: Vec<Value>) -> Vec<Value> {
        let mut out = Vec::with_capacity(events.len());
        for event in events {
            if self.rng.chance(4) {
                continue;
            }
            if self.rng.chance(3) {
                out.push(event.clone());
            }
            out.push(event);
        }
        out
    }

    /// Splits text at up to three random character boundaries.
    fn chunks(&mut self, text: &str) -> Vec<String> {
        let boundaries: Vec<usize> = text.char_indices().map(|(i, _)| i).skip(1).collect();
        let mut cuts = Vec::new();
        if !boundaries.is_empty() {
            for _ in 0..self.rng.below(4) {
                cuts.push(self.rng.pick(&boundaries));
            }
        }
        cuts.sort_unstable();
        cuts.dedup();
        let mut pieces = Vec::new();
        let mut start = 0;
        for cut in cuts.into_iter().chain([text.len()]) {
            pieces.push(text[start..cut].to_owned());
            start = cut;
        }
        pieces
    }
}

fn event(kind: &str, index: &Option<Value>, fields: Vec<(&str, Value)>) -> Value {
    let mut all = vec![("type", json!(kind))];
    if let Some(index) = index {
        all.push(("output_index", index.clone()));
    }
    all.extend(fields);
    to_object(all)
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
            serde_json::from_str::<Value>(&a.request).expect("generated request is valid JSON");
        }
        let (streams, finals) = event_cases(7, 200);
        let (again, _) = event_cases(7, 200);
        for (a, b) in streams.iter().zip(&again) {
            assert_eq!(a.request, b.request);
            assert_eq!(a.events, b.events);
        }
        assert_eq!(finals.len(), 200);
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

        let requests = outputs(Translator::ChatRequest, &request_cases(1, 2000));
        check(
            &requests,
            &[
                r#""type":"custom_tool_call""#,
                r#""type":"custom_tool_call_output""#,
                r#""type":"function_call_output""#,
                "call_missing_",
                r#""type":"input_image""#,
                r#""type":"input_file""#,
                r#""type":"input_audio""#,
                r#""role":"developer""#,
                r#""service_tier":"priority""#,
                r#""service_tier":"ultrafast""#,
                r#""type":"json_schema""#,
                r#""verbosity""#,
                r#""tool_choice":{"type":"custom""#,
                "mcp__",
            ],
        );

        let (streams, finals) = event_cases(1, 2000);
        let streams = outputs(Translator::ChatStream, &streams);
        check(
            &streams,
            &[
                r#""images":["#,
                r#""reasoning_content":""#,
                r#"{\"input\":\""#,
                r#""finish_reason":"tool_calls""#,
                r#""finish_reason":"length""#,
                r#""service_tier":"priority""#,
                r#""created":1700000000"#,
                r#""model":"gpt-5-codex""#,
            ],
        );
        let finals = outputs(Translator::ChatNonStream, &finals);
        check(
            &finals,
            &[
                r#""images":["#,
                r#""reasoning_content":""#,
                r#"{\"input\":\""#,
                r#""service_tier":"priority""#,
                r#""created":"(now)""#,
            ],
        );
    }
}
