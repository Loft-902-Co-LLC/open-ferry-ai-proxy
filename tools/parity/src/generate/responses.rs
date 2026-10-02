//! Seeded random input for the Responses translators.
//!
//! Requests mix the fields upstream sets, drops or renames with loosely typed
//! values, and put cache breakpoints everywhere one can go. Event streams come
//! from the Codex stream generator ([`super::response`]), with the model in
//! `response.created` and `response.in_progress` removed or mangled, and the
//! client's model given in different places, or not at all.

use std::ops::{Deref, DerefMut};

use serde_json::{Value, json};

use super::{EFFORTS, Rng, SERVICE_TIERS, escape_text, to_object};
use crate::cases::Case;

/// Builds `count` random Responses requests.
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
    let (streams, finals) = super::response::cases(!seed, count);
    streams
        .into_iter()
        .zip(finals)
        .enumerate()
        .map(|(index, (stream, last))| {
            let mut generator = Generator::new(!seed, index as u64);
            let model = generator.rng.pick(&["gpt-5", "gpt-5-codex", "", "", " "]);
            let request = generator.model_source();
            let translated_request = if generator.rng.chance(30) {
                String::new()
            } else {
                generator.model_source()
            };
            let lines = stream
                .events
                .iter()
                .flat_map(|line| generator.event_lines(line))
                .collect();
            let body = generator.final_body(&last.events[0]);
            let case = |events| Case {
                translated_request: translated_request.clone(),
                events,
                ..Case::new(format!("random-{seed}-{index}"), model, request.clone())
            };
            (case(lines), case(vec![body]))
        })
        .unzip()
}

const WEB_SEARCH_TYPES: &[&str] = &[
    "web_search_preview",
    "web_search_preview",
    "web_search_preview_2025_03_11",
    "web_search",
    "WEB_SEARCH_PREVIEW",
    " web_search_preview",
    "web_search_preview_2025_03_12",
];

const SERVICE_TIERS_CODEX: &[&str] = &["ultrafast", "ULTRAFAST", " UltraFast ", "ultra fast"];

const ROLES: &[&str] = &[
    "user",
    "user",
    "assistant",
    "system",
    "system",
    "developer",
    "System",
    " system",
];

/// Blank and nearly blank arguments: Go's `TrimSpace` and Rust's `trim` must
/// agree on which are blank.
const ARGUMENTS: &[&str] = &[
    "",
    " ",
    "\n\t ",
    "\u{a0}",
    "\u{3000}",
    "\u{200b}",
    "{}",
    " {} ",
    "{\"city\":\"Paris\"}",
    "not json",
];

/// Model names in a request. Upstream skips names that are blank once
/// trimmed, but reports the others untrimmed.
const MODEL_NAMES: &[&str] = &[
    "gpt-5",
    "gpt-5.1-codex",
    "",
    " ",
    "\t\n",
    "\u{a0}",
    "\u{200b}",
    " gpt-5 ",
    "gpt-5(high)",
    "模型",
];

/// The Claude request generator, for its leaf values: text, numbers and
/// loosely typed values.
struct Generator(super::Generator);

impl Deref for Generator {
    type Target = super::Generator;

    fn deref(&self) -> &super::Generator {
        &self.0
    }
}

impl DerefMut for Generator {
    fn deref_mut(&mut self) -> &mut super::Generator {
        &mut self.0
    }
}

impl Generator {
    fn new(seed: u64, index: u64) -> Self {
        Self(super::Generator {
            // A different mix from the other generators', so cases don't share
            // their random choices.
            rng: Rng(seed.rotate_left(32) ^ index.wrapping_mul(0x9FB2_1C65_1E98_DF25)),
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
        })
    }

    // --- Requests ---

    fn request(&mut self) -> Value {
        let mut fields = Vec::new();
        if self.rng.chance(80) {
            fields.push(("model", self.model().into()));
        }
        if self.rng.chance(95) {
            fields.push(("input", self.input()));
        }
        if self.rng.chance(30) {
            fields.push(("instructions", self.loose_text()));
        }
        if self.rng.chance(45) {
            fields.push(("tools", self.tools()));
        }
        if self.rng.chance(30) {
            fields.push(("tool_choice", self.tool_choice()));
        }
        for key in ["stream", "store", "parallel_tool_calls"] {
            if self.rng.chance(40) {
                fields.push((key, self.flag()));
            }
        }
        if self.rng.chance(40) {
            fields.push(("include", self.include()));
        }
        for key in [
            "max_output_tokens",
            "max_completion_tokens",
            "temperature",
            "top_p",
        ] {
            if self.rng.chance(15) {
                fields.push((key, self.number()));
            }
        }
        if self.rng.chance(30) {
            fields.push(("service_tier", self.service_tier()));
        }
        if self.rng.chance(35) {
            let effort = self.loose_choice(EFFORTS);
            fields.push(("reasoning", json!({ "effort": effort, "summary": "auto" })));
        }
        let extras = [
            ("truncation", json!("auto")),
            ("prompt_cache_options", json!({ "retention": "24h" })),
            ("prompt_cache_retention", json!("24h")),
            ("prompt_cache_key", json!("cache-key")),
            (
                "context_management",
                json!([{ "type": "compaction", "compact_threshold": 1000 }]),
            ),
            ("user", json!("user_123")),
            (
                "text",
                json!({ "format": { "type": "text" }, "verbosity": "low" }),
            ),
            ("metadata", json!({ "user_id": "user_123" })),
            ("previous_response_id", json!("resp_123")),
            // Only breakpoints in `input` are removed.
            ("prompt_cache_breakpoint", json!({ "type": "ephemeral" })),
        ];
        for (key, value) in extras {
            if self.rng.chance(12) {
                fields.push((key, value));
            }
        }
        self.rng.shuffle(&mut fields);
        to_object(fields)
    }

    fn input(&mut self) -> Value {
        match self.rng.below(100) {
            0..=14 => self.text().into(),
            15..=19 => self.one_of(&[
                Value::Null,
                json!(5),
                json!({ "role": "user" }),
                json!(true),
            ]),
            _ => {
                let count = self.rng.below(7);
                Value::Array((0..count).map(|_| self.input_item()).collect())
            }
        }
    }

    fn input_item(&mut self) -> Value {
        let mut item = match self.rng.below(100) {
            0..=39 => self.message(true),
            40..=54 => self.function_call(),
            55..=69 => self.function_call_output(),
            70..=79 => json!({
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{ "type": "summary_text", "text": self.text() }],
                "encrypted_content": "gAAAAABsignature"
            }),
            80..=84 => {
                let input = self.rng.pick(&["", " ", "*** Begin Patch"]);
                json!({ "type": "custom_tool_call", "call_id": "call_2", "name": "apply_patch", "input": input })
            }
            85..=89 => json!({ "type": "item_reference", "id": "msg_1" }),
            90..=94 => self.message(false),
            _ => {
                return self.one_of(&[json!("hi"), json!(5), Value::Null, json!(["system"])]);
            }
        };
        if self.rng.chance(8) {
            let breakpoint = self.breakpoint();
            item["prompt_cache_breakpoint"] = breakpoint;
        }
        item
    }

    fn message(&mut self, typed: bool) -> Value {
        let mut fields = Vec::new();
        if typed {
            fields.push(("type", json!("message")));
        }
        let role = if self.rng.chance(95) {
            self.rng.pick(ROLES).into()
        } else {
            self.one_of(&[json!(5), Value::Null, json!(["system"]), json!("SYSTEM")])
        };
        fields.push(("role", role));
        fields.push(("content", self.content()));
        self.object(fields)
    }

    fn content(&mut self) -> Value {
        match self.rng.below(100) {
            0..=24 => self.text().into(),
            25..=29 => self.one_of(&[Value::Null, json!(5), json!({ "text": "object" })]),
            _ => {
                let count = self.rng.below(4);
                Value::Array((0..count).map(|_| self.part()).collect())
            }
        }
    }

    fn part(&mut self) -> Value {
        if self.rng.chance(5) {
            return self.one_of(&[json!("text"), json!(5), Value::Null]);
        }
        let kind = self.loose_choice(&["input_text", "output_text", "refusal"]);
        let mut fields = vec![("type", kind), ("text", self.loose_text())];
        if self.rng.chance(15) {
            fields.push(("prompt_cache_breakpoint", self.breakpoint()));
        }
        self.object(fields)
    }

    fn breakpoint(&mut self) -> Value {
        self.one_of(&[
            json!({ "type": "ephemeral" }),
            json!({ "type": "ephemeral" }),
            json!(true),
            Value::Null,
            json!("auto"),
        ])
    }

    fn function_call(&mut self) -> Value {
        let arguments = if self.rng.chance(85) {
            self.rng.pick(ARGUMENTS).into()
        } else {
            self.one_of(&[Value::Null, json!(5), json!({ "a": 1 }), json!([])])
        };
        let mut fields = vec![
            ("type", json!("function_call")),
            ("call_id", json!("call_1")),
            ("name", json!("get_weather")),
            ("arguments", arguments),
        ];
        if self.rng.chance(5) {
            fields.push(("role", json!("system")));
        }
        self.object(fields)
    }

    fn function_call_output(&mut self) -> Value {
        let output = if self.rng.chance(50) {
            self.loose_text()
        } else {
            let count = self.rng.below(3);
            Value::Array((0..count).map(|_| self.part()).collect())
        };
        self.object(vec![
            ("type", json!("function_call_output")),
            ("call_id", json!("call_1")),
            ("output", output),
        ])
    }

    fn tools(&mut self) -> Value {
        if self.rng.chance(5) {
            return self.one_of(&[json!({}), json!("tools"), Value::Null]);
        }
        let count = self.rng.below(4);
        Value::Array((0..count).map(|_| self.tool()).collect())
    }

    fn tool(&mut self) -> Value {
        match self.rng.below(10) {
            0..=4 => {
                let mut tool = json!({
                    "type": "function",
                    "name": "get_weather",
                    "parameters": { "type": "object", "properties": { "city": { "type": "string" } } }
                });
                if self.rng.chance(5) {
                    tool["prompt_cache_breakpoint"] = self.breakpoint();
                }
                tool
            }
            5..=7 => {
                let mut fields = vec![("type", self.rng.pick(WEB_SEARCH_TYPES).into())];
                if self.rng.chance(30) {
                    fields.push(("search_context_size", json!("low")));
                }
                self.object(fields)
            }
            8 => json!({ "type": "custom", "name": "apply_patch" }),
            _ => self.one_of(&[
                json!("web_search_preview"),
                json!(5),
                Value::Null,
                json!({ "type": 5 }),
                json!({ "type": ["web_search_preview"] }),
                json!({ "name": "web_search_preview" }),
            ]),
        }
    }

    fn tool_choice(&mut self) -> Value {
        match self.rng.below(10) {
            0..=2 => self.one_of(&[
                json!("auto"),
                json!("none"),
                json!("required"),
                json!("web_search_preview"),
            ]),
            3..=4 => json!({ "type": self.rng.pick(WEB_SEARCH_TYPES) }),
            5 => json!({ "type": "function", "name": "get_weather" }),
            6..=7 => {
                let tools = vec![self.tool(), self.tool()];
                json!({ "type": "allowed_tools", "mode": "auto", "tools": tools })
            }
            8 => {
                let tools = vec![self.tool()];
                json!({ "type": self.rng.pick(WEB_SEARCH_TYPES), "tools": tools })
            }
            _ => self.one_of(&[
                Value::Null,
                json!(5),
                json!([{ "type": "web_search_preview" }]),
                json!({ "tools": "web_search_preview" }),
            ]),
        }
    }

    /// A field Codex requires to be a particular boolean.
    fn flag(&mut self) -> Value {
        if self.rng.chance(60) {
            self.rng.chance(50).into()
        } else {
            self.bool_like()
        }
    }

    fn include(&mut self) -> Value {
        if self.rng.chance(50) {
            return json!(["reasoning.encrypted_content"]);
        }
        self.one_of(&[
            json!([]),
            json!(["file_search_call.results"]),
            json!([
                "reasoning.encrypted_content",
                "message.output_text.logprobs"
            ]),
            json!([
                "message.output_text.logprobs",
                "reasoning.encrypted_content"
            ]),
            json!("reasoning.encrypted_content"),
            json!([" reasoning.encrypted_content"]),
            json!([["reasoning.encrypted_content"]]),
            json!([5]),
            Value::Null,
        ])
    }

    fn service_tier(&mut self) -> Value {
        match self.rng.below(100) {
            0..=59 => self.rng.pick(SERVICE_TIERS).into(),
            60..=89 => self.rng.pick(SERVICE_TIERS_CODEX).into(),
            _ => self.one_of(&[json!(5), Value::Null, json!(true), json!({})]),
        }
    }

    // --- Event streams ---

    /// A request, as text, for a response translator to read the model from.
    fn model_source(&mut self) -> String {
        if self.rng.chance(5) {
            return self.rng.pick(&["", "not json", "[]", "null"]).to_owned();
        }
        let mut fields = vec![("input", json!("hi"))];
        if self.rng.chance(70) {
            fields.push(("model", self.model_name()));
        }
        if self.rng.chance(20) {
            let request = if self.rng.chance(80) {
                json!({ "model": self.model_name() })
            } else {
                self.one_of(&[Value::Null, json!("gpt-5"), json!([{ "model": "gpt-5" }])])
            };
            fields.push(("request", request));
        }
        self.rng.shuffle(&mut fields);
        let request = to_object(fields);
        self.render(&request)
    }

    fn model_name(&mut self) -> Value {
        if self.rng.chance(85) {
            self.rng.pick(MODEL_NAMES).into()
        } else {
            self.one_of(&[
                Value::Null,
                json!(5),
                json!(true),
                json!({ "id": "gpt-5" }),
                json!(["gpt-5"]),
            ])
        }
    }

    /// One line of a Codex stream, with a `response.created` or
    /// `response.in_progress` event varied, and now and then another one added.
    fn event_lines(&mut self, line: &str) -> Vec<String> {
        let start = line
            .strip_prefix("data:")
            .and_then(|data| serde_json::from_str::<Value>(data.trim()).ok())
            .filter(|event| {
                matches!(
                    event["type"].as_str(),
                    Some("response.created" | "response.in_progress")
                )
            });
        let mut lines = vec![match start {
            Some(mut event) if self.rng.chance(85) => {
                self.vary_start(&mut event);
                self.data_line(&event)
            }
            _ => line.to_owned(),
        }];
        if self.rng.chance(2) {
            let kind = self.rng.pick(&["response.created", "response.in_progress"]);
            let mut event = json!({ "type": kind, "response": { "id": "resp_2", "status": "in_progress", "model": "gpt-5" } });
            self.vary_start(&mut event);
            lines.push(self.data_line(&event));
        }
        lines
    }

    fn vary_start(&mut self, event: &mut Value) {
        let Value::Object(fields) = event else {
            return;
        };
        let roll = self.rng.below(100);
        if roll < 80
            && let Some(Value::Object(response)) = fields.get_mut("response")
        {
            response.shift_remove("model");
        }
        match roll {
            55..=64 => {
                if let Some(Value::Object(response)) = fields.get_mut("response") {
                    let model = self.one_of(&[Value::Null, json!(""), json!(5), json!(" ")]);
                    response.insert("model".into(), model);
                }
            }
            65..=74 => {
                let response = self.one_of(&[
                    Value::Null,
                    json!("resp_1"),
                    json!(5),
                    json!([]),
                    json!([{ "id": "resp_1" }]),
                    json!(true),
                    json!({}),
                ]);
                fields.insert("response".into(), response);
            }
            75..=79 => {
                fields.shift_remove("response");
            }
            80..=84 => {
                let kind = self.one_of(&[
                    json!("Response.Created"),
                    json!("response.created "),
                    json!("response.completed"),
                    json!(5),
                    Value::Null,
                ]);
                fields.insert("type".into(), kind);
            }
            _ => {}
        }
    }

    /// An SSE data line, with the spacing varied, or now and then bare JSON.
    fn data_line(&mut self, event: &Value) -> String {
        let mut text = event.to_string();
        if self.rng.chance(10) {
            text = escape_text(&text);
        }
        match self.rng.below(100) {
            0..=79 => format!("data: {text}"),
            80..=87 => format!("data:{text}"),
            88..=91 => format!("data:\t {text} \r"),
            // Go's TrimSpace and Rust's trim both remove a no-break space.
            92..=94 => format!("data: {text}\u{a0}"),
            _ => text,
        }
    }

    /// Codex's final event for the non-streaming translator, sometimes
    /// replaced by its response alone or with the fields it is read by mangled.
    fn final_body(&mut self, last: &str) -> String {
        let Ok(mut event) = serde_json::from_str::<Value>(last) else {
            return last.to_owned();
        };
        if let Value::Object(fields) = &mut event {
            match self.rng.below(100) {
                0..=14 => {
                    if let Some(response @ Value::Object(_)) = fields.get_mut("response") {
                        event = response.take();
                    }
                }
                15..=19 => {
                    let response =
                        self.one_of(&[Value::Null, json!(5), json!("resp_1"), json!([])]);
                    fields.insert("response".into(), response);
                }
                20..=22 => {
                    fields.shift_remove("response");
                }
                23..=27 => {
                    let kind = self.one_of(&[
                        json!(""),
                        Value::Null,
                        json!("response.incomplete"),
                        json!("response.failed"),
                        json!(5),
                    ]);
                    fields.insert("type".into(), kind);
                }
                28..=30 => {
                    let output = self.one_of(&[json!([]), json!({}), json!("output")]);
                    fields.shift_remove("type");
                    fields.insert("output".into(), output);
                }
                _ => {}
            }
        }
        self.render(&event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert_eq!(a.events, b.events);
            assert_eq!(a.translated_request, b.translated_request);
        }
        assert_eq!(finals.len(), 200);
    }

    /// Guards against a generator that never reaches the translators' branches.
    #[test]
    fn cases_cover_the_translators_branches() {
        let requests = request_cases(1, 2000);
        let count = |needle: &str| {
            requests
                .iter()
                .filter(|case| case.request.contains(needle))
                .count()
        };
        for needle in [
            "\"input\":\"",
            "\"role\":\"system\"",
            "\"arguments\":\"\"",
            "prompt_cache_breakpoint",
            "web_search_preview",
            "\"service_tier\":\"ultrafast\"",
            "\"service_tier\":\"fast\"",
            "\"stream\":false",
            "\"include\":[]",
        ] {
            assert!(
                count(needle) >= 20,
                "{needle} in {} requests",
                count(needle)
            );
        }

        let (streams, finals) = event_cases(1, 2000);
        let filled = streams
            .iter()
            .filter(|case| {
                case.events
                    .iter()
                    .any(|line| line.contains("response.created") && !line.contains("\"model\""))
            })
            .count();
        assert!(
            filled >= 200,
            "{filled} streams with a created event naming no model"
        );
        let bare = finals
            .iter()
            .filter(|case| !case.events[0].contains("\"type\""))
            .count();
        assert!(bare >= 100, "{bare} final bodies without a type");
    }
}
