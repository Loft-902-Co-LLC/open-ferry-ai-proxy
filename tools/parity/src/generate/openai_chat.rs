//! Seeded random input for the translators from a Chat Completions upstream:
//! Chat Completions streams and whole responses for the response translators
//! to Claude, Responses and Chat Completions clients, and requests for the
//! Chat Completions passthrough.
//!
//! Streams are chunks as OpenAI-compatible providers send them: a role chunk
//! or none, text and reasoning (`reasoning_content`, `reasoning` or
//! `reasoning_details`) cut at character boundaries, tool calls whose ID,
//! name and arguments come in separate chunks, interleaved with each other or
//! with text, a second choice, a finish reason with usage, before it or
//! without it, usage-only chunks, error events, `[DONE]` early, twice or
//! never, and lines that aren't data. Whole responses carry the same in one
//! message, its content sometimes a list of parts, with fields missing or of
//! the wrong type.
//!
//! Requests for the passthrough are the Chat Completions generator's
//! ([`super::chat`]), their `model` the one asked for, another, missing or
//! not a string.

use std::ops::{Deref, DerefMut};

use serde_json::{Value, json};

use super::{Rng, escape_text, num, to_object};
use crate::cases::Case;

/// Builds `count` random Chat Completions requests for the passthrough.
pub fn request_cases(seed: u64, count: usize) -> Vec<Case> {
    super::chat::request_cases(seed.rotate_left(18), count)
        .into_iter()
        .enumerate()
        .map(|(index, case)| {
            let mut generator = Generator::new(seed.rotate_left(18), index as u64);
            let mut request: Value =
                serde_json::from_str(&case.request).expect("generated requests are JSON");
            let model = generator.rng.pick(MODELS);
            if let Some(fields) = request.as_object_mut() {
                let current = match generator.rng.below(10) {
                    0..=3 => Some(json!(model)),
                    4 => None,
                    5 => Some(generator.one_of(&[
                        json!(5),
                        Value::Null,
                        json!([model]),
                        json!({ "name": model }),
                    ])),
                    6 => Some(json!(format!(" {model}"))),
                    _ => fields.get("model").cloned(),
                };
                match current {
                    Some(current) => {
                        fields.insert("model".into(), current);
                    }
                    None => {
                        fields.shift_remove("model");
                    }
                }
            }
            let text = generator.render(&request);
            Case::new(format!("random-{seed}-{index}"), model, text)
        })
        .collect()
}

/// Builds `count` random Chat Completions streams for the passthrough, and a
/// non-streaming case from a whole response for each.
pub fn event_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    let calls: [Call; 2] = [
        ("get_weather".to_owned(), ARGUMENTS),
        ("search".to_owned(), ARGUMENTS),
    ];
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed.rotate_left(9), index);
            let lines = generator.stream(&calls);
            let body = generator.body(&calls);
            let case = |events| Case::response(format!("random-{seed}-{index}"), "", events);
            (case(lines), case(vec![body]))
        })
        .unzip()
}

/// Model names as providers report them, which the passthrough compares
/// with the one asked for.
const MODELS: &[&str] = &[
    "gpt-4o",
    "gpt-4o",
    "gpt-4o-2024-08-06",
    "deepseek-chat",
    "qwen3-coder-plus",
    "",
    " Model ",
];

/// Function call arguments: objects, other JSON, broken JSON and nothing.
pub(super) const ARGUMENTS: &[&str] = &[
    r#"{"city":"Paris"}"#,
    r#"{"path":"C:\\temp\\a.txt","lines":[1,2]}"#,
    r#"{"q":"café 🚀","n":1.50}"#,
    "{ \"spaced\" : true }",
    "{}",
    "",
    " ",
    "[1,2]",
    "\"text\"",
    r#"{"a":"#,
    r#"{"a":1"#,
    "not json",
];

/// Token counts read as integers, none out of int64's range: Go converts
/// those by the CPU's rules. No negative zero either, which a usage object
/// repeated as it is writes as Go doesn't.
const INTEGERS: &[&str] = &["1.5", "2.0", "1e3", "9007199254740993", "-7", "0.5"];

/// A tool a call may name, and the arguments it may be given.
pub(super) type Call = (String, &'static [&'static str]);

/// The Chat Completions response generator. It builds on the Claude request
/// generator for its leaf values: text, numbers and IDs.
pub(super) struct Generator {
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
    pub(super) fn new(seed: u64, index: u64) -> Self {
        Self {
            base: super::Generator {
                // A different mix from the other generators', so cases don't
                // share their random choices.
                rng: Rng(seed ^ index.wrapping_mul(0x8EBC_6AF0_9C88_C6E3)),
                tool_names: Vec::new(),
                tool_use_ids: Vec::new(),
            },
        }
    }

    // --- Streams ---

    /// A Chat Completions stream, as its lines, whose calls name the tools in
    /// `calls`.
    pub(super) fn stream(&mut self, calls: &[Call]) -> Vec<String> {
        let header = self.header("chat.completion.chunk", true);
        let mut chunks = Vec::new();
        if self.rng.chance(60) {
            let delta = if self.rng.chance(80) {
                json!({ "role": "assistant", "content": "" })
            } else {
                json!({ "role": "assistant" })
            };
            let choice = self.choice(json!(0), delta, None);
            chunks.push(chunk(&header, Some(vec![choice])));
        }
        for (index, delta) in self.deltas(calls) {
            let choice = self.choice(index, delta, None);
            chunks.push(chunk(&header, Some(vec![choice])));
        }

        let finish = self.finish_reason();
        let usage = self.usage();
        let delta = if self.rng.chance(85) {
            json!({})
        } else {
            json!({ "content": self.text() })
        };
        let last = self.choice(json!(0), delta, Some(finish));
        match self.rng.below(10) {
            0..=2 => {
                let mut last = chunk(&header, Some(vec![last]));
                last["usage"] = usage;
                chunks.push(last);
            }
            3 | 4 => {
                chunks.push(chunk(&header, Some(vec![last])));
                chunks.push(self.usage_chunk(&header, usage));
            }
            5 | 6 => chunks.push(chunk(&header, Some(vec![last]))),
            7 => chunks.push(self.usage_chunk(&header, usage)),
            8 => {
                chunks.push(self.usage_chunk(&header, usage));
                chunks.push(chunk(&header, Some(vec![last])));
            }
            _ => {}
        }

        let mut lines = Vec::new();
        for chunk in &chunks {
            if self.rng.chance(4) {
                lines.push(self.other_line());
            }
            lines.push(self.line(chunk));
        }
        match self.rng.below(20) {
            0..=14 => lines.push("data: [DONE]".to_owned()),
            15 => {
                lines.push("data: [DONE]".to_owned());
                lines.push("data: [DONE]".to_owned());
            }
            16 => {
                lines.push("data: [DONE]".to_owned());
                let choice = self.choice(json!(0), json!({ "content": "late" }), None);
                let late = chunk(&header, Some(vec![choice]));
                lines.push(self.line(&late));
            }
            17 => lines.push("data:[DONE]\r".to_owned()),
            _ => {}
        }
        lines
    }

    /// The fields every chunk or response has: its ID, object, creation time
    /// and model. With `odd_ids`, the ID may be something other than a
    /// string, which a provider's stream can carry but a pretty-printed whole
    /// response can't without its JSON text reaching the output.
    fn header(&mut self, object: &str, odd_ids: bool) -> Vec<(&'static str, Value)> {
        let mut fields = Vec::new();
        if self.rng.chance(95) {
            let id = match self.rng.below(25) {
                0 => json!(""),
                1 => json!("resp_1"),
                2 => json!("gen-1759400000-a/b"),
                3 if odd_ids => json!(5),
                4 if odd_ids => json!({ "n": 1 }),
                5 if odd_ids => Value::Null,
                _ => json!(format!("chatcmpl-{}", self.alphanumeric(6))),
            };
            fields.push(("id", id));
        }
        if self.rng.chance(92) {
            let object = if self.rng.chance(92) {
                json!(object)
            } else {
                self.one_of(&[
                    json!("chat.completion"),
                    json!("chat.completion.chunk"),
                    json!(""),
                    Value::Null,
                    json!(5),
                ])
            };
            fields.push(("object", object));
        }
        if self.rng.chance(90) {
            let created = match self.rng.below(10) {
                0 => json!("1700000000"),
                1 => json!(0),
                2 => num("1.7e9"),
                3 => Value::Null,
                _ => json!(1_700_000_000),
            };
            fields.push(("created", created));
        }
        if self.rng.chance(90) {
            let model = self.loose_choice(MODELS);
            fields.push(("model", model));
        }
        if self.rng.chance(15) {
            fields.push(("system_fingerprint", json!("fp_44709d6fcb")));
        }
        fields
    }

    /// A streamed choice. Its `finish_reason` is null or left out unless
    /// `finish` gives one.
    fn choice(&mut self, index: Value, delta: Value, finish: Option<Option<Value>>) -> Value {
        let mut fields = vec![("index", index), ("delta", delta)];
        match finish {
            Some(Some(reason)) => fields.push(("finish_reason", reason)),
            _ if self.rng.chance(70) => fields.push(("finish_reason", Value::Null)),
            _ => {}
        }
        to_object(fields)
    }

    /// The deltas of a response's content, each with its choice's index:
    /// reasoning, text and tool calls, one after another or interleaved.
    fn deltas(&mut self, calls: &[Call]) -> Vec<(Value, Value)> {
        let mut parts: Vec<Vec<(Value, Value)>> = Vec::new();
        let mut call_index = 0;
        for _ in 0..self.rng.below(5) {
            let deltas = match self.rng.below(10) {
                0..=2 => self.reasoning_deltas(),
                3..=5 => self.text_deltas(),
                _ => {
                    call_index += 1;
                    self.call_deltas(call_index - 1, calls)
                }
            };
            parts.push(deltas.into_iter().map(|delta| (json!(0), delta)).collect());
        }
        if self.rng.chance(8) {
            // A second choice, which only some translators tell apart.
            let index = self.one_of(&[json!(1), json!(1), json!("1")]);
            let deltas = if self.rng.chance(50) {
                self.text_deltas()
            } else {
                self.call_deltas(0, calls)
            };
            parts.push(
                deltas
                    .into_iter()
                    .map(|delta| (index.clone(), delta))
                    .collect(),
            );
        }
        if !self.rng.chance(25) {
            return parts.into_iter().flatten().collect();
        }
        // Interleaved: each delta from a random part that has some left.
        let mut deltas = Vec::new();
        let mut parts: Vec<std::vec::IntoIter<(Value, Value)>> =
            parts.into_iter().map(Vec::into_iter).collect();
        while !parts.is_empty() {
            let at = self.rng.below(parts.len());
            match parts[at].next() {
                Some(delta) => deltas.push(delta),
                None => {
                    parts.remove(at);
                }
            }
        }
        deltas
    }

    fn reasoning_deltas(&mut self) -> Vec<Value> {
        let key = self.rng.pick(&[
            "reasoning_content",
            "reasoning_content",
            "reasoning",
            "reasoning_details",
        ]);
        let text = self.text();
        let mut deltas = Vec::new();
        for piece in self.pieces(&text) {
            let value = if key == "reasoning_details" {
                json!([{ "type": "reasoning.text", "text": piece }])
            } else if self.rng.chance(3) {
                self.one_of(&[json!(5), Value::Null, json!({ "text": piece })])
            } else {
                json!(piece)
            };
            deltas.push(to_object(vec![(key, value)]));
        }
        deltas
    }

    fn text_deltas(&mut self) -> Vec<Value> {
        let text = self.text();
        let mut deltas = Vec::new();
        for piece in self.pieces(&text) {
            let content = if self.rng.chance(3) {
                self.one_of(&[json!(5), Value::Null, json!({ "text": "x" }), json!(["a"])])
            } else {
                json!(piece)
            };
            deltas.push(json!({ "content": content }));
        }
        deltas
    }

    /// The deltas of one tool call: its ID, name and arguments in one delta,
    /// or spread over several in the orders providers send them.
    fn call_deltas(&mut self, index: usize, calls: &[Call]) -> Vec<Value> {
        let (name, arguments) = self.call_name(calls);
        let id = match self.rng.below(20) {
            0 => None,
            1 => Some(json!("")),
            2 => Some(json!("call 1/ü")),
            3 => Some(json!(5)),
            _ => Some(json!(format!("call_{}", self.alphanumeric(8)))),
        };
        let index = match self.rng.below(20) {
            0 => None,
            1 => Some(json!(index.to_string())),
            _ => Some(json!(index)),
        };
        let mut pieces: Vec<Value> = self
            .pieces(&arguments)
            .into_iter()
            .map(Value::String)
            .collect();
        if self.rng.chance(2) {
            let at = self.rng.below(pieces.len());
            pieces[at] = json!({ "city": "Paris" });
        }

        let delta = |id: Option<&Value>, function: Vec<(&str, Value)>| -> Value {
            let mut call = Vec::new();
            if let Some(index) = &index {
                call.push(("index", index.clone()));
            }
            if let Some(id) = id {
                call.push(("id", id.clone()));
                call.push(("type", json!("function")));
            }
            if !function.is_empty() {
                call.push(("function", to_object(function)));
            }
            json!({ "tool_calls": [to_object(call)] })
        };
        let joined = Value::String(
            pieces
                .iter()
                .map(|piece| piece.as_str().unwrap_or("{}"))
                .collect(),
        );
        let mut deltas = Vec::new();
        match self.rng.below(5) {
            0 => deltas.push(delta(
                id.as_ref(),
                vec![("name", name.clone()), ("arguments", joined)],
            )),
            1 => {
                deltas.push(delta(
                    id.as_ref(),
                    vec![("name", name.clone()), ("arguments", json!(""))],
                ));
                for piece in pieces {
                    deltas.push(delta(None, vec![("arguments", piece)]));
                }
            }
            2 => {
                // The name before the ID, which some providers send apart.
                deltas.push(delta(None, vec![("name", name.clone())]));
                let mut pieces = pieces.into_iter();
                let first = pieces.next().unwrap_or_else(|| json!(""));
                deltas.push(delta(id.as_ref(), vec![("arguments", first)]));
                for piece in pieces {
                    deltas.push(delta(None, vec![("arguments", piece)]));
                }
            }
            3 => {
                // The name again in a later delta, or sent empty.
                let mut pieces = pieces.into_iter();
                let first = pieces.next().unwrap_or_else(|| json!(""));
                deltas.push(delta(
                    id.as_ref(),
                    vec![("name", name.clone()), ("arguments", first)],
                ));
                let again = self.one_of(&[name.clone(), json!(""), json!("other_name")]);
                deltas.push(delta(None, vec![("name", again), ("arguments", json!(""))]));
                for piece in pieces {
                    deltas.push(delta(None, vec![("arguments", piece)]));
                }
            }
            _ => {
                deltas.push(delta(id.as_ref(), Vec::new()));
                deltas.push(delta(None, vec![("name", name.clone())]));
                for piece in pieces {
                    deltas.push(delta(None, vec![("arguments", piece)]));
                }
            }
        }
        deltas
    }

    /// A tool's name for a call, and its arguments: usually one in `calls`,
    /// sometimes one that names no tool or isn't a name.
    fn call_name(&mut self, calls: &[Call]) -> (Value, String) {
        if calls.is_empty() || self.rng.chance(6) {
            let name = self.one_of(&[json!("undeclared_tool"), json!(""), Value::Null, json!(5)]);
            return (name, self.rng.pick(ARGUMENTS).to_owned());
        }
        let (name, arguments) = self.rng.pick(calls);
        (json!(name), self.rng.pick(arguments).to_owned())
    }

    /// A finish reason, or `None` for none at all.
    fn finish_reason(&mut self) -> Option<Value> {
        Some(match self.rng.below(20) {
            0..=6 => json!("stop"),
            7..=10 => json!("tool_calls"),
            11 | 12 => json!("length"),
            13 => json!("content_filter"),
            14 => json!("function_call"),
            15 => json!(""),
            16 => json!("STOP"),
            17 => json!(5),
            18 => Value::Null,
            _ => return None,
        })
    }

    /// A chunk with usage and no choice: an empty `choices`, none, or one
    /// with an empty delta.
    fn usage_chunk(&mut self, header: &[(&'static str, Value)], usage: Value) -> Value {
        let choices = match self.rng.below(10) {
            0..=6 => Some(Vec::new()),
            7 | 8 => None,
            _ => Some(vec![self.choice(json!(0), json!({}), None)]),
        };
        let mut usage_chunk = chunk(header, choices);
        usage_chunk["usage"] = usage;
        usage_chunk
    }

    /// Token counts, with the details providers add, or something else.
    fn usage(&mut self) -> Value {
        if self.rng.chance(5) {
            return self.one_of(&[
                Value::Null,
                json!({}),
                json!(5),
                json!({ "input_tokens": 3, "output_tokens": 4 }),
            ]);
        }
        let mut fields = Vec::new();
        if self.rng.chance(90) {
            fields.push(("prompt_tokens", self.count()));
        }
        if self.rng.chance(90) {
            fields.push(("completion_tokens", self.count()));
        }
        if self.rng.chance(75) {
            fields.push(("total_tokens", self.count()));
        }
        if self.rng.chance(30) {
            let mut details = Vec::new();
            if self.rng.chance(80) {
                details.push(("cached_tokens", self.count()));
            }
            if self.rng.chance(25) {
                details.push(("cache_write_tokens", self.count()));
            }
            if self.rng.chance(25) {
                details.push(("cache_creation_tokens", self.count()));
            }
            let details = self.object(details);
            fields.push(("prompt_tokens_details", details));
        }
        if self.rng.chance(20) {
            let reasoning = self.count();
            fields.push((
                "completion_tokens_details",
                json!({ "reasoning_tokens": reasoning }),
            ));
        }
        if self.rng.chance(10) {
            let reasoning = self.count();
            fields.push((
                "output_tokens_details",
                json!({ "reasoning_tokens": reasoning }),
            ));
        }
        if self.rng.chance(5) {
            fields.push(("output_tokens", self.count()));
        }
        self.object(fields)
    }

    fn count(&mut self) -> Value {
        match self.rng.below(10) {
            0 => num(self.rng.pick(INTEGERS)),
            1 => self.one_of(&[json!("12"), Value::Null]),
            _ => json!(self.rng.pick(&[0, 1, 12, 100, 2048, 123_456])),
        }
    }

    /// `text` cut at up to two random character boundaries, sometimes with an
    /// empty piece after.
    fn pieces(&mut self, text: &str) -> Vec<String> {
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

    /// A chunk as a stream line: usually `data: <JSON>`, sometimes spaced or
    /// escaped differently, or bare.
    fn line(&mut self, chunk: &Value) -> String {
        let json = chunk.to_string();
        match self.rng.below(100) {
            0..=79 => format!("data: {json}"),
            80..=84 => format!("data:{json}"),
            85..=89 => format!("data: {}", escape_text(&json)),
            90..=92 => format!("data:  {json} \r"),
            _ => json,
        }
    }

    /// A line that carries no chunk, or ends the stream early.
    fn other_line(&mut self) -> String {
        self.rng
            .pick(&[
                "",
                "",
                ": keep-alive",
                "event: chunk",
                "data:",
                "data: ",
                " data: {}",
                "data: {not json",
                r#"data: {"error":{"message":"Rate limit reached","type":"rate_limit_error"}}"#,
                r#"data: {"object":"chat.completion.chunk"}"#,
                "[DONE]",
                "data: [DONE] ",
            ])
            .to_owned()
    }

    // --- Whole responses ---

    /// A whole Chat Completions response whose calls name the tools in
    /// `calls`.
    pub(super) fn body(&mut self, calls: &[Call]) -> String {
        if self.rng.chance(3) {
            return self
                .rng
                .pick(&["", "{not json", "null", "[]", "{}", "data: {}"])
                .to_owned();
        }
        let mut fields = self.header("chat.completion", false);
        if self.rng.chance(95) {
            fields.push(("choices", self.choices(calls)));
        }
        if self.rng.chance(70) {
            fields.push(("usage", self.usage()));
        }
        let body = to_object(fields);
        match self.rng.below(10) {
            0 => serde_json::to_string_pretty(&body).expect("a Value always serializes"),
            1 => escape_text(&body.to_string()),
            _ => body.to_string(),
        }
    }

    fn choices(&mut self, calls: &[Call]) -> Value {
        match self.rng.below(20) {
            0 => json!([]),
            1 => json!({ "0": self.message_choice(json!(0), calls) }),
            2 => json!("choices"),
            3 => json!([
                self.message_choice(json!(0), calls),
                self.message_choice(json!(1), calls)
            ]),
            _ => json!([self.message_choice(json!(0), calls)]),
        }
    }

    fn message_choice(&mut self, index: Value, calls: &[Call]) -> Value {
        let mut message = vec![("role", json!("assistant"))];
        match self.rng.below(10) {
            0..=5 => message.push(("content", json!(self.text()))),
            6 => message.push(("content", Value::Null)),
            7 => message.push(("content", self.content_parts(calls))),
            8 => {
                let content = self.one_of(&[json!(5), json!(true)]);
                message.push(("content", content));
            }
            _ => {}
        }
        if self.rng.chance(30) {
            let key = self.rng.pick(&[
                "reasoning_content",
                "reasoning_content",
                "reasoning",
                "reasoning_details",
            ]);
            let text = self.text();
            let value = match key {
                "reasoning_details" => json!([{ "type": "reasoning.text", "text": text }]),
                _ => json!(text),
            };
            message.push((key, value));
        }
        if self.rng.chance(40) {
            let calls: Vec<Value> = (0..1 + self.rng.below(3))
                .map(|_| self.whole_call(calls))
                .collect();
            message.push(("tool_calls", Value::Array(calls)));
        }
        if self.rng.chance(10) {
            message.push(("refusal", Value::Null));
        }
        let message = self.object(message);
        let mut fields = vec![("index", index), ("message", message)];
        match self.finish_reason() {
            Some(reason) => fields.push(("finish_reason", reason)),
            None if self.rng.chance(50) => fields.push(("finish_reason", Value::Null)),
            None => {}
        }
        if self.rng.chance(10) {
            fields.push(("logprobs", Value::Null));
        }
        to_object(fields)
    }

    /// Message content as a list of parts, of the kinds some providers send.
    fn content_parts(&mut self, calls: &[Call]) -> Value {
        let parts = (0..1 + self.rng.below(4))
            .map(|_| match self.rng.below(6) {
                0 | 1 => json!({ "type": "text", "text": self.text() }),
                2 => json!({ "type": "reasoning", "text": self.text() }),
                3 => json!({ "type": "tool_calls", "tool_calls": [self.whole_call(calls)] }),
                4 => json!({ "type": "image_url", "image_url": { "url": "https://example.com/a.png" } }),
                _ => json!({ "text": "no type" }),
            })
            .collect();
        Value::Array(parts)
    }

    fn whole_call(&mut self, calls: &[Call]) -> Value {
        let (name, arguments) = self.call_name(calls);
        let mut fields = Vec::new();
        match self.rng.below(20) {
            0 => {}
            1 => fields.push(("id", json!(""))),
            2 => fields.push(("id", json!("call 1/ü"))),
            _ => fields.push(("id", json!(format!("call_{}", self.alphanumeric(8))))),
        }
        if self.rng.chance(90) {
            fields.push(("type", json!("function")));
        }
        let mut function = vec![("name", name)];
        match self.rng.below(20) {
            0 => {}
            1 => function.push(("arguments", json!({ "city": "Paris" }))),
            _ => function.push(("arguments", json!(arguments))),
        }
        fields.push(("function", to_object(function)));
        to_object(fields)
    }
}

/// A chunk with `header`'s fields and `choices`, if any.
fn chunk(header: &[(&'static str, Value)], choices: Option<Vec<Value>>) -> Value {
    let mut fields = header.to_vec();
    if let Some(choices) = choices {
        fields.push(("choices", Value::Array(choices)));
    }
    to_object(fields)
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
        let requests = outputs(Translator::OpenAIChatRequest, &cases);
        check(&requests, &[r#""model":"gpt-4o""#, r#""model":" Model ""#]);
        let unchanged = cases
            .iter()
            .filter(|case| {
                let request: Value = serde_json::from_str(&case.request).unwrap();
                request["model"] == case.model.as_str()
            })
            .count();
        assert!(
            unchanged >= 200,
            "{unchanged} requests already name the model"
        );

        let (streams, finals) = event_cases(1, 2000);
        let streams = outputs(Translator::OpenAIChatStream, &streams);
        check(
            &streams,
            &[
                r#"\"tool_calls\":[{\"index\":0,\"id\":\"call_"#,
                r#"\"finish_reason\":\"tool_calls\""#,
                r#"\"usage\":{"#,
                r#"\"choices\":[],"#,
                r#""""#,
                "rate_limit_error",
            ],
        );
        let finals = outputs(Translator::OpenAIChatNonStream, &finals);
        check(
            &finals,
            &[r#"\"choices\":[{\"index\":0"#, r#"\n  \"choices\""#],
        );
    }
}
