//! Seeded random input for the Chat Completions → Claude translators.
//!
//! Requests start as the Chat Completions generator's ([`super::chat`]) and
//! gain what only the Claude translator reads: `cache_control` markers on
//! messages, parts and tools, assistant `reasoning_content`, data URLs,
//! `top_p`, `stop`, token limits, client user IDs, reasoning summary switches,
//! Claude effort levels and `allowed_tools` choices.
//!
//! Event streams are Claude's: text, thinking and tool use blocks streamed in
//! deltas, usage split between `message_start` and `message_delta`, loosely
//! typed indexes and counts, and lines that aren't events.

use std::ops::{Deref, DerefMut};

use serde_json::{Value, json};

use super::{EFFORTS, Rng, escape_text, num, to_object};
use crate::cases::Case;

/// Builds `count` random Chat Completions requests for a Claude upstream.
pub fn request_cases(seed: u64, count: usize) -> Vec<Case> {
    super::chat::request_cases(seed.rotate_left(29), count)
        .into_iter()
        .enumerate()
        .map(|(index, case)| {
            let mut generator = Generator::new(seed, index as u64);
            let mut request: Value =
                serde_json::from_str(&case.request).expect("generated requests are JSON");
            generator.request(&mut request);
            let model = generator.rng.pick(MODELS);
            let text = generator.render(&request);
            Case::new(format!("random-{seed}-{index}"), model, text)
        })
        .collect()
}

/// Builds `count` random Claude event streams, and a non-streaming case from
/// the whole of each.
pub fn event_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed.rotate_left(41), index);
            let model =
                generator
                    .rng
                    .pick(&["claude-opus-4-6", "claude-sonnet-4-5-20250929", "", " "]);
            let events = generator.events();
            let lines: Vec<String> = events.iter().map(|event| generator.line(event)).collect();
            let body = generator.body(&events);
            let case = |events| Case {
                events,
                ..Case::new(format!("random-{seed}-{index}"), model, "")
            };
            (case(lines), case(vec![body]))
        })
        .unzip()
}

/// Claude models with effort levels, with budgets only, or unknown, and names
/// the catalog lookup has to clean up.
const MODELS: &[&str] = &[
    "claude-opus-4-6",
    "claude-opus-4-6",
    "claude-sonnet-4-6",
    "claude-opus-4-7",
    "claude-opus-5-5",
    "claude-fable-5-1",
    "claude-sonnet-4-5-20250929",
    "claude-sonnet-4-5-20250929",
    "claude-haiku-4-5-20251001",
    "claude-3-5-haiku-20241022",
    "claude-opus-4-6-thinking",
    "claude-opus-4-6(high)",
    " claude-opus-4-6 ",
    "gpt-5",
    "",
];

/// Loosely written integers, none out of int64's range: Go converts those by
/// the CPU's rules, and the result then decides more than the one field.
const INTEGERS: &[&str] = &["1.5", "2.0", "-0", "1e3", "9007199254740993", "-7", "0.5"];

/// Tool arguments as Claude streams them, whole or cut short.
const ARGUMENTS: &[&str] = &[
    r#"{"city":"Paris"}"#,
    r#"{"path":"C:\\temp\\a.txt","lines":[1,2]}"#,
    r#"{"q":"café 🚀","n":1.50}"#,
    "{}",
    "",
    r#"{"a":"#,
    "not json",
];

/// The Claude request generator, for its leaf values: text, numbers, tool
/// names and schemas.
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
                rng: Rng(seed.rotate_left(36) ^ index.wrapping_mul(0x9FB2_1C65_1E98_DF25)),
                tool_names: Vec::new(),
                tool_use_ids: Vec::new(),
            },
        }
    }

    // --- Requests ---

    /// Adds what only the Claude translator reads to a Chat Completions request.
    fn request(&mut self, request: &mut Value) {
        let Value::Object(fields) = request else {
            return;
        };
        if let Some(Value::Array(tools)) = fields.get_mut("tools") {
            for tool in tools.iter_mut() {
                self.tool(tool);
            }
            self.tool_names = tools
                .iter()
                .filter_map(|tool| tool.get("function")?.get("name").cloned())
                .collect();
        }
        if let Some(Value::Array(messages)) = fields.get_mut("messages") {
            for message in messages {
                self.message(message);
            }
        }
        let mut extras = Vec::new();
        if self.rng.chance(25) {
            extras.push(("reasoning_effort", self.effort()));
        }
        if self.rng.chance(20) {
            extras.push(self.summary());
        }
        if self.rng.chance(20) {
            extras.push(("top_p", self.top_p()));
        }
        if self.rng.chance(15) {
            extras.push(("stop", self.stop()));
        }
        if self.rng.chance(15) {
            extras.push(("max_completion_tokens", self.token_limit()));
        }
        if self.rng.chance(10) {
            extras.push(("max_tokens", self.token_limit()));
        }
        if self.rng.chance(15) {
            extras.push(("metadata", self.metadata()));
        }
        if self.rng.chance(12) {
            let user = self.loose_choice(&["user_123", "", " ", " padded "]);
            extras.push(("user", user));
        }
        if self.rng.chance(15) {
            extras.push(("tool_choice", self.tool_choice()));
        }
        if self.rng.chance(20) {
            let parallel = self.one_of(&[
                json!(false),
                json!(false),
                json!(true),
                json!("false"),
                json!(0),
                Value::Null,
            ]);
            extras.push(("parallel_tool_calls", parallel));
        }
        for (key, value) in extras {
            fields.insert(key.to_owned(), value);
        }
    }

    fn tool(&mut self, tool: &mut Value) {
        let Value::Object(fields) = tool else {
            return;
        };
        if self.rng.chance(15) {
            fields.insert("cache_control".into(), self.cache_control());
        }
        if self.rng.chance(8) {
            fields.insert("strict".into(), self.bool_like());
        }
        if let Some(Value::Object(function)) = fields.get_mut("function") {
            if self.rng.chance(10) {
                function.insert("cache_control".into(), self.cache_control());
            }
            if self.rng.chance(10)
                && let Some(parameters) = function.shift_remove("parameters")
            {
                function.insert("parametersJsonSchema".into(), parameters);
            }
        }
    }

    fn message(&mut self, message: &mut Value) {
        let Value::Object(fields) = message else {
            return;
        };
        if self.rng.chance(15) {
            fields.insert("cache_control".into(), self.cache_control());
        }
        let role = fields
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if role == "assistant" && self.rng.chance(30) {
            let reasoning = match self.rng.below(5) {
                0 => json!(" "),
                1 => self.loose_text(),
                _ => self.text().into(),
            };
            fields.insert("reasoning_content".into(), reasoning);
        }
        if let Some(Value::Array(parts)) = fields.get_mut("content") {
            for part in parts.iter_mut() {
                if self.rng.chance(15)
                    && let Value::Object(part) = part
                {
                    part.insert("cache_control".into(), self.cache_control());
                }
            }
            if role == "user" && self.rng.chance(25) {
                let part = self.data_part();
                let at = self.rng.below(parts.len() + 1);
                parts.insert(at, part);
            }
        }
    }

    /// An image or file part, usually with a data URL, in the forms upstream
    /// splits by hand.
    fn data_part(&mut self) -> Value {
        if self.rng.chance(50) {
            let url = self.rng.pick(&[
                "data:image/png;base64,iVBORw0KGgo=",
                "data:image/jpeg;base64,/9j/4AAQ",
                "data:;base64,iVBORw0KGgo=",
                "data:image/png,iVBORw0KGgo=",
                "data:image/png;base64",
                "data:,",
                "https://example.com/cat.png",
            ]);
            json!({ "type": "image_url", "image_url": { "url": url } })
        } else {
            let data = self.rng.pick(&[
                "data:application/pdf;base64,JVBERi0=",
                "data:text/plain;charset=utf-8;base64,aGk=",
                "data:application/pdf,JVBE;Ri0=",
                "data:;base64,JVBERi0=",
                "data:application/pdf;base64",
                "JVBERi0=",
            ]);
            json!({ "type": "file", "file": { "file_data": data, "filename": "a.pdf" } })
        }
    }

    fn cache_control(&mut self) -> Value {
        match self.rng.below(10) {
            0..=5 => json!({ "type": "ephemeral" }),
            6 => json!({ "type": "ephemeral", "ttl": "1h" }),
            7 => json!({ "type": "persistent" }),
            _ => self.one_of(&[
                json!("ephemeral"),
                Value::Null,
                json!({}),
                json!({ "type": 5 }),
            ]),
        }
    }

    fn effort(&mut self) -> Value {
        match self.rng.below(10) {
            0..=5 => self.loose_choice(&[
                "none", "auto", "minimal", "low", "medium", "high", "xhigh", "max",
            ]),
            6..=7 => self.loose_choice(&[" High ", "MAX", "", "ultra", "Auto", " none "]),
            _ => self.loose_choice(EFFORTS),
        }
    }

    /// One of the fields that show or hide reasoning summaries.
    fn summary(&mut self) -> (&'static str, Value) {
        let flag = self.bool_like();
        match self.rng.below(9) {
            0 | 1 => ("reasoning", json!({ "summary": self.summary_setting() })),
            2 => (
                "reasoning",
                json!({ "generate_summary": self.summary_setting() }),
            ),
            3 => ("reasoning", json!({ "exclude": flag })),
            4 => ("reasoning", json!({ "enabled": flag })),
            5 => ("include_reasoning", flag),
            6 => (
                "extra_body",
                json!({ "google": { "thinking_config": { "include_thoughts": flag } } }),
            ),
            7 => ("thinking", json!({ "include_thoughts": flag })),
            _ => (
                "generationConfig",
                json!({ "thinkingConfig": { "includeThoughts": flag } }),
            ),
        }
    }

    fn summary_setting(&mut self) -> Value {
        if self.rng.chance(10) {
            return Value::Null;
        }
        self.loose_choice(&["auto", "concise", "detailed", "none", " Detailed ", "bogus"])
    }

    /// A `top_p`, never one too large for a float: upstream would write that
    /// as `+Inf`, which isn't JSON.
    fn top_p(&mut self) -> Value {
        match self.rng.below(10) {
            0..=4 => self.one_of(&[
                num("0.9"),
                num("1"),
                num("0"),
                num("0.95"),
                num("1e-3"),
                num("0.10"),
            ]),
            5 => self.number(),
            6..=7 => self.loose_choice(&["0.5", "1", " 0.5", "abc", "", "+1", ".5", "5.", "1e-7"]),
            _ => self.one_of(&[
                json!(true),
                json!(false),
                Value::Null,
                json!({}),
                json!([0.5]),
            ]),
        }
    }

    fn stop(&mut self) -> Value {
        match self.rng.below(10) {
            0..=3 => self.loose_text(),
            4..=6 => {
                let count = self.rng.below(4);
                Value::Array((0..count).map(|_| self.loose_text()).collect())
            }
            _ => self.one_of(&[
                json!([]),
                json!([""]),
                Value::Null,
                json!({ "a": 1 }),
                json!(["\n\n", "END"]),
            ]),
        }
    }

    /// A token limit, never out of int64's range: it decides whether thinking
    /// fits (see [`INTEGERS`]).
    fn token_limit(&mut self) -> Value {
        match self.rng.below(10) {
            0..=5 => self.one_of(&[json!(1024), json!(64000), json!(1), json!(0), json!(-5)]),
            6..=7 => num(self.rng.pick(INTEGERS)),
            _ => self.one_of(&[json!("2048"), Value::Null, json!(true), json!(" 10 ")]),
        }
    }

    fn metadata(&mut self) -> Value {
        match self.rng.below(10) {
            0..=4 => {
                let id = self.loose_choice(&["user-123", "", " ", " padded "]);
                json!({ "user_id": id })
            }
            5..=6 => json!({ "user_id": "user-123", "session": "s" }),
            _ => self.one_of(&[
                json!({}),
                json!("meta"),
                Value::Null,
                json!([{ "user_id": "x" }]),
            ]),
        }
    }

    /// Usually an `allowed_tools` choice, in each of its shapes.
    fn tool_choice(&mut self) -> Value {
        if self.rng.chance(20) {
            // Other forms, for parallel_tool_calls to combine with.
            return self.one_of(&[
                json!("none"),
                json!("auto"),
                json!("required"),
                json!({ "type": "function", "function": { "name": "get_weather" } }),
                json!(["required"]),
                json!({ "type": "none" }),
            ]);
        }
        let count = self.rng.below(4);
        let tools: Vec<Value> = (0..count).map(|_| self.allowed_tool()).collect();
        let tools = match tools.as_slice() {
            [tool] if self.rng.chance(20) => tool.clone(),
            _ => Value::Array(tools),
        };
        let mode =
            self.loose_choice(&["auto", "required", "none", "Required", " AUTO ", "any", ""]);
        match self.rng.below(4) {
            0 | 1 => {
                json!({ "type": "allowed_tools", "allowed_tools": { "mode": mode, "tools": tools } })
            }
            2 => json!({ "type": "allowed_tools", "mode": mode, "tools": tools }),
            _ => json!({ "type": "allowed_tools", "tools": tools }),
        }
    }

    fn allowed_tool(&mut self) -> Value {
        let names = self.tool_names.clone();
        let name = if !names.is_empty() && self.rng.chance(70) {
            self.rng.pick(&names)
        } else {
            self.tool_name()
        };
        let name = match name {
            Value::String(text) if self.rng.chance(15) => format!(" {text} ").into(),
            name => name,
        };
        match self.rng.below(3) {
            0 => json!({ "type": "function", "function": { "name": name } }),
            1 => json!({ "type": "function", "name": name }),
            _ => json!({ "name": name }),
        }
    }

    // --- Event streams ---

    fn events(&mut self) -> Vec<Value> {
        let mut events = Vec::new();
        if self.rng.chance(92) {
            events.push(self.message_start());
        }
        for position in 0..self.rng.below(5) {
            let index = self.block_index(position);
            let block = self.block(&index);
            events.extend(block);
            if self.rng.chance(5) {
                events.push(json!({ "type": "ping" }));
            }
        }
        if self.rng.chance(85) {
            events.push(self.message_delta());
        }
        if self.rng.chance(85) {
            events.push(json!({ "type": "message_stop" }));
        }
        if self.rng.chance(6) {
            let at = self.rng.below(events.len() + 1);
            let error = self.error();
            events.insert(at, error);
        }
        if events.len() > 1 && self.rng.chance(8) {
            let (a, b) = (self.rng.below(events.len()), self.rng.below(events.len()));
            events.swap(a, b);
        }
        events
    }

    fn message_start(&mut self) -> Value {
        if self.rng.chance(4) {
            return self.one_of(&[
                json!({ "type": "message_start" }),
                json!({ "type": "message_start", "message": "msg" }),
            ]);
        }
        let mut message = Vec::new();
        match self.rng.below(10) {
            0 => {}
            1 => message.push(("id", self.one_of(&[json!(""), json!(5), Value::Null]))),
            _ => message.push(("id", format!("msg_{}", self.alphanumeric(12)).into())),
        }
        message.push(("type", json!("message")));
        message.push(("role", json!("assistant")));
        if self.rng.chance(85) {
            let model = self.loose_choice(&["claude-opus-4-6", "claude-sonnet-4-5-20250929", ""]);
            message.push(("model", model));
        }
        message.push(("content", json!([])));
        message.push(("stop_reason", Value::Null));
        if self.rng.chance(85) {
            let usage = self.usage();
            message.push(("usage", usage));
        }
        let message = self.object(message);
        json!({ "type": "message_start", "message": message })
    }

    fn usage(&mut self) -> Value {
        let mut fields = Vec::new();
        for key in [
            "input_tokens",
            "output_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ] {
            if self.rng.chance(60) {
                let count = self.count();
                fields.push((key, count));
            }
        }
        if self.rng.chance(10) {
            fields.push(("service_tier", json!("standard")));
        }
        self.object(fields)
    }

    /// A token count, never out of int64's range: sums would carry it (see
    /// [`INTEGERS`]).
    fn count(&mut self) -> Value {
        match self.rng.below(100) {
            0..=79 => json!(self.rng.below(50_000)),
            80..=84 => json!(0),
            85..=89 => num(self.rng.pick(INTEGERS)),
            90..=94 => self.one_of(&[json!("12"), Value::Null, json!(true)]),
            _ => self.one_of(&[json!(i64::MAX), json!(-3)]),
        }
    }

    /// A block's `index`: usually its position, sometimes missing or loosely
    /// typed, or another block's.
    fn block_index(&mut self, position: usize) -> Option<Value> {
        Some(match self.rng.below(100) {
            0..=89 => json!(position),
            90..=92 => return None,
            93..=94 => json!(position.to_string()),
            95..=96 => num("1.5"),
            97 => json!(-1),
            _ => json!(position + 1),
        })
    }

    fn block(&mut self, index: &Option<Value>) -> Vec<Value> {
        let (start, deltas) = match self.rng.below(100) {
            0..=34 => (
                json!({ "type": "text", "text": "" }),
                self.deltas("text_delta", "text"),
            ),
            35..=54 => {
                let mut deltas = self.deltas("thinking_delta", "thinking");
                if self.rng.chance(60) {
                    deltas.push(
                        json!({ "type": "signature_delta", "signature": "EqQBCkYIBxgCKkA=" }),
                    );
                }
                (json!({ "type": "thinking", "thinking": "" }), deltas)
            }
            55..=84 => self.tool_use(),
            85..=89 => (
                json!({ "type": "redacted_thinking", "data": "EmwKAhgB" }),
                Vec::new(),
            ),
            90..=93 => (
                json!({ "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {} }),
                vec![json!({ "type": "input_json_delta", "partial_json": "{\"query\":\"x\"}" })],
            ),
            _ => (
                self.one_of(&[
                    json!({}),
                    json!("text"),
                    Value::Null,
                    json!({ "type": "citations" }),
                ]),
                vec![json!({ "type": "citations_delta", "citation": {} })],
            ),
        };
        let mut events = Vec::new();
        if self.rng.chance(95) {
            events.push(event(
                "content_block_start",
                index,
                vec![("content_block", start)],
            ));
        }
        for delta in deltas {
            events.push(event("content_block_delta", index, vec![("delta", delta)]));
        }
        if self.rng.chance(93) {
            events.push(event("content_block_stop", index, Vec::new()));
        }
        events
    }

    /// Text streamed in pieces as `kind` deltas holding `key`, now and then
    /// one that isn't a string.
    fn deltas(&mut self, kind: &str, key: &str) -> Vec<Value> {
        let text = self.text();
        self.chunks(&text)
            .into_iter()
            .map(|chunk| {
                let value = if self.rng.chance(4) {
                    self.loose_text()
                } else {
                    chunk.into()
                };
                to_object(vec![("type", kind.into()), (key, value)])
            })
            .collect()
    }

    fn tool_use(&mut self) -> (Value, Vec<Value>) {
        let mut block = vec![("type", json!("tool_use"))];
        match self.rng.below(100) {
            0..=84 => block.push(("id", format!("toolu_01{}", self.alphanumeric(20)).into())),
            85..=89 => block.push(("id", json!(""))),
            90..=94 => {}
            _ => block.push(("id", self.number())),
        }
        if self.rng.chance(95) {
            let name = self.tool_name();
            block.push(("name", name));
        }
        block.push(("input", json!({})));
        let arguments = self.rng.pick(ARGUMENTS);
        let deltas = if self.rng.chance(85) {
            self.chunks(arguments)
                .into_iter()
                .map(|chunk| json!({ "type": "input_json_delta", "partial_json": chunk }))
                .collect()
        } else {
            Vec::new()
        };
        (to_object(block), deltas)
    }

    fn message_delta(&mut self) -> Value {
        let mut delta = Vec::new();
        if self.rng.chance(90) {
            let reason = self.loose_choice(&[
                "end_turn",
                "end_turn",
                "tool_use",
                "max_tokens",
                "stop_sequence",
                "refusal",
                "sensitive",
                "pause_turn",
                "",
            ]);
            delta.push(("stop_reason", reason));
        }
        if self.rng.chance(30) {
            delta.push(("stop_sequence", self.one_of(&[Value::Null, json!("END")])));
        }
        let mut fields = vec![
            ("type", json!("message_delta")),
            ("delta", to_object(delta)),
        ];
        if self.rng.chance(85) {
            let usage = self.usage();
            fields.push(("usage", usage));
        }
        to_object(fields)
    }

    fn error(&mut self) -> Value {
        self.one_of(&[
            json!({ "type": "error", "error": { "type": "overloaded_error", "message": "Overloaded" } }),
            json!({ "type": "error", "error": { "message": "bad" } }),
            json!({ "type": "error" }),
        ])
    }

    /// `text` cut at one or two random character boundaries, sometimes with an
    /// empty piece after.
    fn chunks(&mut self, text: &str) -> Vec<String> {
        let boundaries: Vec<usize> = text.char_indices().skip(1).map(|(at, _)| at).collect();
        let mut cuts = Vec::new();
        if !boundaries.is_empty() {
            for _ in 0..self.rng.below(3) {
                cuts.push(self.rng.pick(&boundaries));
            }
        }
        cuts.sort_unstable();
        cuts.dedup();
        let mut pieces = Vec::new();
        let mut from = 0;
        for cut in cuts {
            pieces.push(text[from..cut].to_owned());
            from = cut;
        }
        pieces.push(text[from..].to_owned());
        if self.rng.chance(5) {
            pieces.push(String::new());
        }
        pieces
    }

    /// An event as a stream line: usually `data: <JSON>`, sometimes spaced or
    /// escaped differently, and now and then a line that isn't data.
    fn line(&mut self, event: &Value) -> String {
        let json = event.to_string();
        match self.rng.below(100) {
            0..=79 => format!("data: {json}"),
            80..=84 => format!("data:{json}"),
            85..=87 => format!("data: {}", escape_text(&json)),
            88..=89 => format!("data:  {json} \r"),
            90..=91 => json,
            92..=93 => format!("event: {}", event_type(event)),
            94 => String::new(),
            95 => "data: [DONE]".to_owned(),
            96 => ": keep-alive".to_owned(),
            97 => format!(" data: {json}"),
            _ => "data: {not json".to_owned(),
        }
    }

    /// The whole stream as one SSE body, for the non-streaming translator.
    fn body(&mut self, events: &[Value]) -> String {
        let newline = if self.rng.chance(10) { "\r\n" } else { "\n" };
        let mut lines = Vec::new();
        for event in events {
            if self.rng.chance(40) {
                lines.push(format!("event: {}", event_type(event)));
            }
            lines.push(self.line(event));
            if self.rng.chance(50) {
                lines.push(String::new());
            }
        }
        lines.join(newline)
    }
}

fn event(kind: &str, index: &Option<Value>, fields: Vec<(&str, Value)>) -> Value {
    let mut all = vec![("type", json!(kind))];
    if let Some(index) = index {
        all.push(("index", index.clone()));
    }
    all.extend(fields);
    to_object(all)
}

fn event_type(event: &Value) -> &str {
    event.get("type").and_then(Value::as_str).unwrap_or("event")
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
            assert_eq!(a.model, b.model);
            serde_json::from_str::<Value>(&a.request).expect("generated request is valid JSON");
        }
        let (streams, finals) = event_cases(7, 200);
        let (again, finals_again) = event_cases(7, 200);
        for (a, b) in streams.iter().zip(&again) {
            assert_eq!(a.events, b.events);
        }
        for (a, b) in finals.iter().zip(&finals_again) {
            assert_eq!(a.events, b.events);
        }
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

        let cases = request_cases(1, 2000);
        let requests = outputs(Translator::ClaudeChatRequest, &cases);
        check(
            &requests,
            &[
                r#""cache_control":{"type":"ephemeral""#,
                r#""type":"tool_result""#,
                r#""type":"tool_use""#,
                r#""type":"image""#,
                r#""type":"document""#,
                r#""type":"url""#,
                r#""stop_sequences""#,
                r#""top_p""#,
                r#""user_id""#,
                r#""type":"adaptive""#,
                r#""budget_tokens""#,
                r#""effort""#,
                r#""display":"summarized""#,
                r#""display":"omitted""#,
                r#""disable_parallel_tool_use":true"#,
                r#""tool_choice":{"type":"any""#,
                r#""tool_choice":{"type":"tool""#,
                r#""strict":true"#,
                "toolu_(generated-1)",
                "JSON Schema:",
            ],
        );
        let compat = outputs(Translator::ClaudeChatRequestCompat, &cases);
        check(&compat, &[r#""type":"thinking""#]);

        let (streams, finals) = event_cases(1, 2000);
        let streams = outputs(Translator::ClaudeChatStream, &streams);
        check(
            &streams,
            &[
                r#""role":"assistant""#,
                r#""reasoning_content":""#,
                r#""tool_calls":[{"#,
                r#""finish_reason":"tool_calls""#,
                r#""finish_reason":"length""#,
                r#""finish_reason":"content_filter""#,
                r#""cached_tokens""#,
                r#""choices":[]"#,
                r#""error""#,
                r#""created":"(now)""#,
            ],
        );
        let finals = outputs(Translator::ClaudeChatNonStream, &finals);
        check(
            &finals,
            &[
                r#""reasoning_content":""#,
                r#""tool_calls":[{"#,
                r#""finish_reason":"tool_calls""#,
                r#""cached_tokens""#,
                r#""created":"(now)""#,
            ],
        );
    }
}
