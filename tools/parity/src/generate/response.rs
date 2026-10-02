//! Seeded random Codex event streams, for the response translators.
//!
//! Each case is a Claude request declaring tools, and the Codex events a reply
//! to it might produce: reasoning, text, function calls and web searches, one
//! item after another or interleaved. Events are sometimes dropped, repeated or
//! loosely typed, and the final event's `output` may list more or fewer items
//! than were streamed, as Codex's often does.

use open_ferry_translate::codex::claude::convert_claude_request_to_codex;
use serde_json::{Value, json};

use super::{Rng, escape_text, num, to_object};
use crate::cases::Case;

/// Builds `count` random stream cases, and a non-streaming case from the final
/// event of each stream.
pub fn cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    let mut streams = Vec::with_capacity(count);
    let mut finals = Vec::with_capacity(count);
    for index in 0..count as u64 {
        let mut generator = Generator::new(seed, index);
        let request = generator.request();
        let (lines, last) = generator.stream();
        let request = request.to_string();
        let name = format!("random-{seed}-{index}");
        finals.push(Case::response(name.clone(), request.clone(), vec![last]));
        streams.push(Case::response(name, request, lines));
    }
    (streams, finals)
}

/// ASCII only: a long name cut mid-character is a known request deviation, so
/// its round trip would differ for that reason alone.
const TOOL_NAMES: &[&str] = &[
    "get_weather",
    "Bash",
    "Read",
    "web_fetch",
    "get_weather_forecast_for_a_city_with_a_rather_long_descriptive_name_v2",
    "get_weather_forecast_for_a_city_with_a_rather_long_descriptive_name_v3",
    "mcp__a_server_with_a_name_long_enough_to_need_shortening__search_files",
    "mcp__another_server_with_a_long_name_for_shortening_purposes__search_files",
];

const TEXTS: &[&str] = &[
    "",
    " ",
    "Hello!",
    "The weather in Paris is 18°C.",
    "line one\nline two\n",
    "quote \" backslash \\ slash /",
    "<b>html</b> & 'entities'",
    "café naïve 日本語 🚀👍🏽",
    "\u{2028}separators\u{2029}",
    "{\"looks\":\"like json\"}",
    "İstanbul ΑΣ",
];

const ARGUMENTS: &[&str] = &[
    r#"{"city":"Paris"}"#,
    r#"{"city": "Zürich", "unit": "celsius"}"#,
    r#"{"a":[1,2.50,{"b":null}],"c":"<&>"}"#,
    r#"{"q":"café \/ x"}"#,
    r#"{"n":1e400}"#,
    "{}",
    " {\"padded\": true} ",
    "[]",
    "[1,2]",
    "\"text\"",
    "5",
    "not json",
    "{\"unterminated\":",
    "{\"a\":1}{\"b\":2}",
    "",
];

const STOP_REASONS: &[&str] = &[
    "stop",
    "completed",
    "max_tokens",
    "max_output_tokens",
    "tool_calls",
    "function_call",
    "content_filter",
    "pause_turn",
    "refusal",
    "model_context_window_exceeded",
    "end_turn",
    "stop_sequence",
    "something_new",
    "",
];

/// Token counts, kept as text so floats and huge integers reach the translator as written.
const TOKENS: &[&str] = &[
    "0",
    "1",
    "12",
    "100",
    "4096",
    "-3",
    "2.5",
    "9223372036854775807",
    "99999999999999999999",
    "\"42\"",
    "null",
];

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Reasoning,
    Message,
    FunctionCall,
    WebSearch,
    Other,
}

struct Generator {
    rng: Rng,
    /// Tool names as Codex sees them, from our request translator.
    codex_names: Vec<String>,
    /// Tool names as the client declared them.
    declared_names: Vec<String>,
    sequence: u64,
}

impl Generator {
    fn new(seed: u64, index: u64) -> Self {
        Self {
            // A different mix from the request generator's, so the streams don't
            // share its random choices.
            rng: Rng(!seed ^ index.wrapping_mul(0xA24B_AED4_963E_E407)),
            codex_names: Vec::new(),
            declared_names: Vec::new(),
            sequence: 0,
        }
    }

    fn request(&mut self) -> Value {
        let mut fields = Vec::new();
        if self.rng.chance(75) {
            fields.push(("tools", self.tools()));
        }
        fields.push(("messages", json!([{ "role": "user", "content": "hi" }])));
        let request = to_object(fields);

        let codex = convert_claude_request_to_codex("gpt-5", &request);
        self.codex_names = codex["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
            .collect();
        self.declared_names = request["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
            .collect();
        request
    }

    fn tools(&mut self) -> Value {
        if self.rng.chance(5) {
            return self.rng.pick(&[json!({}), json!("tools"), Value::Null]);
        }
        let count = 1 + self.rng.below(4);
        let tools = (0..count)
            .map(|_| match self.rng.below(20) {
                0 => json!({ "description": "no name" }),
                1 => json!({ "type": "web_search_20250305", "name": "web_search" }),
                2 => json!({ "name": 123, "input_schema": { "type": "object" } }),
                _ => json!({ "name": self.rng.pick(TOOL_NAMES), "input_schema": { "type": "object" } }),
            })
            .collect();
        Value::Array(tools)
    }

    /// The event stream as SSE lines, and the final event's JSON on its own.
    fn stream(&mut self) -> (Vec<String>, String) {
        let response_id = format!("resp_{}", self.id_chars(12));
        let mut events = Vec::new();
        if self.rng.chance(95) {
            let response = json!({ "id": response_id, "object": "response", "model": "gpt-5", "status": "in_progress", "output": [] });
            events.push(self.event(
                "response.created",
                None,
                vec![("response", response.clone())],
            ));
            if self.rng.chance(50) {
                events.push(self.event("response.in_progress", None, vec![("response", response)]));
            }
        }

        let count = self.rng.below(6);
        let mut items = Vec::with_capacity(count);
        let mut item_events = Vec::with_capacity(count);
        for index in 0..count {
            let kind = match self.rng.below(100) {
                0..=24 => Kind::Reasoning,
                25..=54 => Kind::Message,
                55..=84 => Kind::FunctionCall,
                85..=95 => Kind::WebSearch,
                _ => Kind::Other,
            };
            let (item, item_stream) = self.item(kind, index);
            items.push((kind, item));
            item_events.push(item_stream);
        }
        events.extend(self.merge(item_events));

        let final_event = self.final_event(&response_id, &items);
        if let Some(final_event) = &final_event {
            events.push(final_event.clone());
        }
        let mut lines = Vec::with_capacity(events.len() * 2);
        let with_event_lines = self.rng.chance(30);
        for event in &mut events {
            if self.rng.chance(4) {
                self.mangle(event, 0);
            }
            if with_event_lines {
                lines.push(format!("event: {}", event["type"].as_str().unwrap_or("")));
            }
            lines.push(self.data_line(event));
            if self.rng.chance(3) {
                lines.push(
                    self.rng
                        .pick(&["", ": ping", "data:", "data: [DONE]", "id: 7"])
                        .to_owned(),
                );
            }
        }
        if self.rng.chance(20) {
            lines.push("data: [DONE]".into());
        }

        let last = match final_event {
            Some(event) if self.rng.chance(95) => event.to_string(),
            _ => events.last().map(Value::to_string).unwrap_or_default(),
        };
        (lines, last)
    }

    /// One output item's final form and the events that stream it.
    fn item(&mut self, kind: Kind, index: usize) -> (Value, Vec<Value>) {
        let index = self.output_index(index);
        match kind {
            Kind::Reasoning => self.reasoning(index),
            Kind::Message => self.message(index),
            Kind::FunctionCall => self.function_call(index),
            Kind::WebSearch => self.web_search(index),
            Kind::Other => {
                let item = json!({ "type": self.rng.pick(&["image_generation_call", "", "custom_tool_call"]), "id": self.id("ig") });
                let added = self.event(
                    "response.output_item.added",
                    index.as_ref(),
                    vec![("item", item.clone())],
                );
                let done = self.event(
                    "response.output_item.done",
                    index.as_ref(),
                    vec![("item", item.clone())],
                );
                (item, vec![added, done])
            }
        }
    }

    fn reasoning(&mut self, index: Option<Value>) -> (Value, Vec<Value>) {
        let id = self.id("rs");
        let parts: Vec<String> = (0..self.rng.below(4)).map(|_| self.text()).collect();
        let index = index.as_ref();
        let mut events = Vec::new();

        let mut added = json!({ "type": "reasoning", "id": id, "summary": [] });
        if self.rng.chance(30) {
            added["encrypted_content"] = self.signature().into();
        }
        events.push(self.event("response.output_item.added", index, vec![("item", added)]));
        for (summary_index, part) in parts.iter().enumerate() {
            let at = || {
                vec![
                    ("item_id", id.clone()),
                    ("summary_index", summary_index.into()),
                ]
            };
            let mut fields = at();
            fields.push(("part", json!({ "type": "summary_text", "text": "" })));
            events.push(self.event("response.reasoning_summary_part.added", index, fields));
            for chunk in self.chunks(part) {
                let mut fields = at();
                fields.push(("delta", chunk.into()));
                events.push(self.event("response.reasoning_summary_text.delta", index, fields));
            }
            let mut fields = at();
            fields.push(("text", part.as_str().into()));
            events.push(self.event("response.reasoning_summary_text.done", index, fields));
            let mut fields = at();
            fields.push(("part", json!({ "type": "summary_text", "text": part })));
            events.push(self.event("response.reasoning_summary_part.done", index, fields));
        }

        let summary: Value = match self.rng.below(20) {
            0 => json!(parts.concat()),
            1 => json!([{ "kind": "no text field" }, "plain"]),
            _ => parts
                .iter()
                .map(|text| json!({ "type": "summary_text", "text": text }))
                .collect(),
        };
        let mut done = json!({ "type": "reasoning", "id": id, "summary": summary });
        if self.rng.chance(10) {
            done["content"] = json!([{ "type": "reasoning_text", "text": self.text() }]);
        }
        if self.rng.chance(80) {
            done["encrypted_content"] = self.signature().into();
        }
        events.push(self.event(
            "response.output_item.done",
            index,
            vec![("item", done.clone())],
        ));
        (done, events)
    }

    fn message(&mut self, index: Option<Value>) -> (Value, Vec<Value>) {
        let id = self.id("msg");
        let text = self.text();
        let part_type = if self.rng.chance(90) {
            "output_text"
        } else {
            "refusal"
        };
        let index = index.as_ref();
        let mut events = Vec::new();
        let at = |content_index: usize| {
            vec![
                ("item_id", id.clone()),
                ("content_index", content_index.into()),
            ]
        };

        let added = json!({ "type": "message", "id": id, "status": "in_progress", "role": "assistant", "content": [] });
        events.push(self.event("response.output_item.added", index, vec![("item", added)]));
        let mut fields = at(0);
        fields.push(("part", json!({ "type": part_type, "text": "" })));
        events.push(self.event("response.content_part.added", index, fields));
        if self.rng.chance(85) {
            for chunk in self.chunks(&text) {
                let mut fields = at(0);
                fields.push(("delta", chunk.into()));
                events.push(self.event("response.output_text.delta", index, fields));
            }
        }
        let mut fields = at(0);
        fields.push(("text", text.as_str().into()));
        events.push(self.event("response.output_text.done", index, fields));
        let mut fields = at(0);
        fields.push(("part", json!({ "type": part_type, "text": text })));
        events.push(self.event("response.content_part.done", index, fields));

        let content = match self.rng.below(20) {
            0 => json!(text),
            1 => Value::Null,
            2 => json!([
                { "type": part_type, "text": text },
                { "type": "output_text", "text": self.text() }
            ]),
            _ => json!([{ "type": part_type, "text": text, "annotations": [] }]),
        };
        let done = json!({ "type": "message", "id": id, "status": "completed", "role": "assistant", "content": content });
        events.push(self.event(
            "response.output_item.done",
            index,
            vec![("item", done.clone())],
        ));
        (done, events)
    }

    fn function_call(&mut self, index: Option<Value>) -> (Value, Vec<Value>) {
        let id = self.id("fc");
        let call_id = self.call_id();
        let name = self.call_name();
        let arguments = self.rng.pick(ARGUMENTS).to_owned();
        let index = index.as_ref();
        let mut events = Vec::new();

        let item = |name: Option<&Value>, arguments: &str, status: &str| {
            let mut fields = vec![("type", json!("function_call")), ("id", id.clone())];
            if let Some(call_id) = &call_id {
                fields.push(("call_id", call_id.clone()));
            }
            if let Some(name) = name {
                fields.push(("name", name.clone()));
            }
            fields.push(("arguments", arguments.into()));
            fields.push(("status", status.into()));
            to_object(fields)
        };
        // Codex names the call up front, but a stream may leave that to output_item.done.
        let early_name = if self.rng.chance(85) {
            name.as_ref()
        } else {
            None
        };
        events.push(self.event(
            "response.output_item.added",
            index,
            vec![("item", item(early_name, "", "in_progress"))],
        ));
        let keys = |generator: &mut Self| {
            let mut fields = Vec::new();
            if generator.rng.chance(85) {
                fields.push(("item_id", id.clone()));
            }
            if generator.rng.chance(10)
                && let Some(call_id) = &call_id
            {
                fields.push(("call_id", call_id.clone()));
            }
            fields
        };
        for chunk in self.chunks(&arguments) {
            let mut fields = keys(self);
            fields.push(("delta", chunk.into()));
            events.push(self.event("response.function_call_arguments.delta", index, fields));
        }
        if self.rng.chance(85) {
            let mut fields = keys(self);
            fields.push(("arguments", arguments.as_str().into()));
            events.push(self.event("response.function_call_arguments.done", index, fields));
        }
        let done = item(name.as_ref(), &arguments, "completed");
        events.push(self.event(
            "response.output_item.done",
            index,
            vec![("item", done.clone())],
        ));
        (done, events)
    }

    fn web_search(&mut self, index: Option<Value>) -> (Value, Vec<Value>) {
        let id = if self.rng.chance(85) {
            Some(self.id("ws"))
        } else {
            None
        };
        let index = index.as_ref();
        let mut events = Vec::new();
        let with_id = |mut fields: Vec<(&'static str, Value)>, key: &'static str| {
            if let Some(id) = &id {
                fields.insert(1, (key, id.clone()));
            }
            to_object(fields)
        };

        let added = with_id(
            vec![
                ("type", json!("web_search_call")),
                ("status", json!("in_progress")),
            ],
            "id",
        );
        events.push(self.event("response.output_item.added", index, vec![("item", added)]));
        for progress in ["in_progress", "searching", "completed"] {
            let fields = match &id {
                Some(id) if self.rng.chance(80) => vec![("item_id", id.clone())],
                _ => Vec::new(),
            };
            events.push(self.event(
                &format!("response.web_search_call.{progress}"),
                index,
                fields,
            ));
        }

        let mut fields = vec![
            ("type", json!("web_search_call")),
            ("status", json!("completed")),
        ];
        match self.rng.below(10) {
            0 => {}
            1 => fields.push((
                "action",
                json!({ "type": "open_page", "url": "https://example.com/a" }),
            )),
            2 => fields.push(("query", json!(self.query()))),
            _ => fields.push(("action", json!({ "type": "search", "query": self.query() }))),
        }
        if self.rng.chance(40) {
            let results: Vec<Value> = (0..self.rng.below(4))
                .map(|n| match self.rng.below(6) {
                    0 => json!({ "url": " ", "title": "blank url" }),
                    1 => json!({ "url": format!("https://example.com/{n}") }),
                    2 => json!("not an object"),
                    _ => json!({ "url": format!("https://example.com/{n}"), "title": format!(" Result {n} ") }),
                })
                .collect();
            fields.push(("results", results.into()));
        }
        let done = with_id(fields, "id");
        events.push(self.event(
            "response.output_item.done",
            index,
            vec![("item", done.clone())],
        ));
        (done, events)
    }

    /// Runs the items' event lists one after another, or interleaves them as
    /// parallel calls do, keeping each list's own order. Some events are then
    /// dropped or repeated.
    fn merge(&mut self, mut lists: Vec<Vec<Value>>) -> Vec<Value> {
        let mut merged = Vec::new();
        if self.rng.chance(35) {
            for list in &mut lists {
                list.reverse();
            }
            while lists.iter().any(|list| !list.is_empty()) {
                let open: Vec<usize> = (0..lists.len()).filter(|&i| !lists[i].is_empty()).collect();
                let list = self.rng.pick(&open);
                merged.extend(lists[list].pop());
            }
        } else {
            merged = lists.into_iter().flatten().collect();
        }

        let mut out = Vec::with_capacity(merged.len());
        for event in merged {
            if self.rng.chance(4) {
                continue;
            }
            if self.rng.chance(2) {
                out.push(event.clone());
            }
            out.push(event);
        }
        out
    }

    /// The final event, which lists the output again, or an error, or nothing
    /// when the stream was cut off.
    fn final_event(&mut self, response_id: &str, items: &[(Kind, Value)]) -> Option<Value> {
        let kind = match self.rng.below(100) {
            0..=77 => "response.completed",
            78..=87 => "response.incomplete",
            88..=93 => {
                let error = self.error();
                return Some(self.event("error", None, error));
            }
            94..=95 => "response.failed",
            _ => return None,
        };

        let mut output: Vec<Value> = match self.rng.below(10) {
            // Codex's final event often has an empty output.
            0..=2 => Vec::new(),
            3 => items
                .iter()
                .filter(|(kind, _)| *kind == Kind::FunctionCall)
                .map(|(_, item)| item.clone())
                .collect(),
            4 => {
                let mut subset: Vec<Value> = items.iter().map(|(_, item)| item.clone()).collect();
                self.rng.shuffle(&mut subset);
                subset.truncate(self.rng.below(subset.len() + 1));
                subset
            }
            _ => items.iter().map(|(_, item)| item.clone()).collect(),
        };
        if self.rng.chance(10) {
            let index = self.output_index(items.len());
            output.push(self.function_call(index).0);
        }
        if self.rng.chance(5)
            && let Some(item) = output.first_mut()
        {
            item["output_index"] = self.output_index(0).unwrap_or(json!(0));
        }

        let output = if self.rng.chance(2) {
            Value::Object(
                output
                    .into_iter()
                    .enumerate()
                    .map(|(i, item)| (format!("k{i}"), item))
                    .collect(),
            )
        } else {
            Value::Array(output)
        };
        let status = if kind == "response.incomplete" {
            "incomplete"
        } else {
            "completed"
        };
        let mut fields = vec![
            ("id", json!(response_id)),
            ("object", json!("response")),
            (
                "model",
                self.rng
                    .pick(&[json!("gpt-5"), json!("gpt-5.6-luna"), json!(5)]),
            ),
            ("status", json!(status)),
            ("output", output),
        ];
        if let Some(usage) = self.usage() {
            fields.push(("usage", usage));
        }
        if self.rng.chance(20) {
            let reason = if self.rng.chance(90) {
                json!(self.rng.pick(STOP_REASONS))
            } else {
                self.loose_value()
            };
            fields.push(("stop_reason", reason));
        }
        if kind == "response.incomplete" || self.rng.chance(5) {
            let reason = self.rng.pick(&["max_output_tokens", "content_filter", ""]);
            fields.push(("incomplete_details", json!({ "reason": reason })));
        }
        if self.rng.chance(15) {
            let sequence = self.rng.pick(&[
                json!("</done>"),
                json!(""),
                json!(" "),
                json!(5),
                Value::Null,
                json!({ "a": 1 }),
                json!(["x"]),
            ]);
            fields.push(("stop_sequence", sequence));
        }
        let response = to_object(fields);
        Some(self.event(kind, None, vec![("response", response)]))
    }

    fn error(&mut self) -> Vec<(&'static str, Value)> {
        let mut error = Vec::new();
        if self.rng.chance(70) {
            error.push((
                "type",
                self.rng.pick(&[
                    json!("invalid_request"),
                    json!("server_error"),
                    json!(" "),
                    json!(5),
                ]),
            ));
        }
        if self.rng.chance(60) {
            error.push((
                "code",
                self.rng.pick(&[
                    json!("cyber_policy"),
                    json!("rate_limit_exceeded"),
                    json!(""),
                    Value::Null,
                ]),
            ));
        }
        if self.rng.chance(70) {
            error.push((
                "message",
                self.rng.pick(&[
                    json!("Something went wrong."),
                    json!("  "),
                    json!("<b>bad</b>"),
                ]),
            ));
        }
        let mut fields = vec![("error", to_object(error))];
        if self.rng.chance(20) {
            fields.push(("message", json!("top-level message")));
        }
        if self.rng.chance(20) {
            fields.push(("error_type", json!("overloaded_error")));
        }
        fields
    }

    fn usage(&mut self) -> Option<Value> {
        match self.rng.below(100) {
            0..=4 => return None,
            5..=7 => return Some(Value::Null),
            8 => return Some(json!("usage")),
            _ => {}
        }
        let mut input_details = Vec::new();
        if self.rng.chance(70) {
            input_details.push(("cached_tokens", self.tokens()));
        }
        if self.rng.chance(20) {
            input_details.push(("cache_write_tokens", self.tokens()));
        }
        if self.rng.chance(20) {
            input_details.push(("cache_creation_tokens", self.tokens()));
        }
        let mut fields = vec![("input_tokens", self.tokens())];
        if self.rng.chance(85) {
            fields.push(("input_tokens_details", to_object(input_details)));
        }
        fields.push(("output_tokens", self.tokens()));
        if self.rng.chance(70) {
            let reasoning = if self.rng.chance(85) {
                self.tokens()
            } else {
                num(self.rng.pick(&["1e400", "-0", "0.5"]))
            };
            fields.push((
                "output_tokens_details",
                json!({ "reasoning_tokens": reasoning }),
            ));
        }
        fields.push(("total_tokens", self.tokens()));
        Some(to_object(fields))
    }

    /// A Codex event. `output_index` is left out when `index` is `None`, and now and then anyway.
    fn event(&mut self, kind: &str, index: Option<&Value>, fields: Vec<(&str, Value)>) -> Value {
        let mut all = vec![("type", json!(kind))];
        if let Some(index) = index
            && self.rng.chance(97)
        {
            all.push(("output_index", index.clone()));
        }
        all.extend(fields);
        all.push(("sequence_number", self.sequence.into()));
        self.sequence += 1;
        to_object(all)
    }

    /// How one item's events give its `output_index`: usually the number, but
    /// sometimes not at all, or as a string or float.
    fn output_index(&mut self, index: usize) -> Option<Value> {
        match self.rng.below(100) {
            0..=4 => None,
            5..=7 => Some(json!(index.to_string())),
            8..=9 => Some(num(&format!("{index}.0"))),
            _ => Some(json!(index)),
        }
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
        let suffix = if self.rng.chance(5) { " \r" } else { "" };
        format!("{prefix}{text}{suffix}")
    }

    /// Replaces a few fields with values of the wrong type.
    fn mangle(&mut self, value: &mut Value, depth: usize) {
        let Value::Object(fields) = value else {
            return;
        };
        for field in fields.values_mut() {
            if self.rng.chance(8) {
                *field = self.loose_value();
            } else if depth < 3 {
                self.mangle(field, depth + 1);
            }
        }
    }

    fn loose_value(&mut self) -> Value {
        self.rng.pick(&[
            Value::Null,
            json!(5),
            num("1.50"),
            json!(true),
            json!({}),
            json!([]),
            json!(""),
            json!(" padded "),
            json!({ "nested": "object" }),
        ])
    }

    fn call_id(&mut self) -> Option<Value> {
        Some(match self.rng.below(100) {
            0..=59 => json!(format!("call_{}", self.id_chars(24))),
            60..=69 => json!(format!("call_{}", self.id_chars(80))),
            70..=77 => json!("call.1:x y"),
            78..=82 => json!("toolu_01ABCdef"),
            83..=87 => json!(""),
            88..=90 => json!("call_é_ü"),
            91..=92 => json!(123),
            93..=94 => json!(" "),
            _ => return None,
        })
    }

    fn call_name(&mut self) -> Option<Value> {
        let roll = self.rng.below(100);
        Some(match roll {
            0..=69 if !self.codex_names.is_empty() => json!(self.rng.pick(&self.codex_names)),
            0..=79 if !self.declared_names.is_empty() => json!(self.rng.pick(&self.declared_names)),
            0..=87 => json!(self.rng.pick(&["shell", "get_weather", "web_search"])),
            88..=92 => json!(""),
            93..=94 => json!(7),
            _ => return None,
        })
    }

    fn id(&mut self, prefix: &str) -> Value {
        match self.rng.below(100) {
            0..=2 => json!(""),
            3 => json!(9),
            _ => json!(format!("{prefix}_{}", self.id_chars(16))),
        }
    }

    fn id_chars(&mut self, len: usize) -> String {
        const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        (0..len)
            .map(|_| char::from(CHARS[self.rng.below(CHARS.len())]))
            .collect()
    }

    fn text(&mut self) -> String {
        self.rng.pick(TEXTS).to_owned()
    }

    fn query(&mut self) -> String {
        self.rng
            .pick(&["weather in Paris", "  rust serde  ", "", " ", "a < b & c"])
            .to_owned()
    }

    fn signature(&mut self) -> String {
        if self.rng.chance(5) {
            return String::new();
        }
        format!("gAAAAAB{}", self.id_chars(40))
    }

    fn tokens(&mut self) -> Value {
        num(self.rng.pick(TOKENS))
    }

    /// Splits `text` into up to three pieces on character boundaries.
    fn chunks(&mut self, text: &str) -> Vec<String> {
        let boundaries: Vec<usize> = text.char_indices().map(|(i, _)| i).skip(1).collect();
        let mut cuts: Vec<usize> = (0..self.rng.below(3))
            .filter(|_| !boundaries.is_empty())
            .map(|_| self.rng.pick(&boundaries))
            .collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_are_reproducible_and_valid_json() {
        let (first, first_finals) = cases(3, 200);
        let (second, _) = cases(3, 200);
        for (a, b) in first.iter().zip(&second) {
            assert_eq!(a.events, b.events);
            serde_json::from_str::<Value>(&a.request).expect("request is valid JSON");
        }
        for case in &first_finals {
            if !case.events[0].is_empty() {
                serde_json::from_str::<Value>(&case.events[0]).expect("final event is valid JSON");
            }
        }
    }

    #[test]
    fn chunks_rejoin_to_the_text() {
        let mut generator = Generator::new(1, 1);
        for text in TEXTS {
            assert_eq!(generator.chunks(text).concat(), *text);
        }
    }

    /// Guards against the streams turning degenerate: translated, they should
    /// produce every kind of Claude block, stop reason and error.
    #[test]
    fn streams_cover_the_translators_output() {
        use std::collections::BTreeMap;

        use crate::translator::Translator;

        let (streams, finals) = cases(1, 1000);
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for case in &streams {
            let frames = Translator::Stream
                .run_rust(case)
                .expect("streams translate");
            for frame in frames.as_array().expect("frames") {
                let data = &frame["data"];
                let key = match frame["event"].as_str() {
                    Some("content_block_start") => {
                        format!("block {}", data["content_block"]["type"])
                    }
                    Some("content_block_delta") => format!("delta {}", data["delta"]["type"]),
                    Some("message_delta") => format!("stop {}", data["delta"]["stop_reason"]),
                    Some(event) => event.to_owned(),
                    None => "unparsed".to_owned(),
                };
                *seen.entry(key).or_default() += 1;
            }
        }
        for case in &finals {
            let message = Translator::NonStream
                .run_rust(case)
                .expect("finals translate");
            for block in message["content"].as_array().into_iter().flatten() {
                *seen.entry(format!("final {}", block["type"])).or_default() += 1;
            }
        }

        for key in [
            "block \"text\"",
            "block \"thinking\"",
            "block \"tool_use\"",
            "block \"server_tool_use\"",
            "block \"web_search_tool_result\"",
            "delta \"text_delta\"",
            "delta \"thinking_delta\"",
            "delta \"signature_delta\"",
            "delta \"input_json_delta\"",
            "stop \"end_turn\"",
            "stop \"tool_use\"",
            "stop \"max_tokens\"",
            "error",
            "final \"text\"",
            "final \"thinking\"",
            "final \"tool_use\"",
            "final \"server_tool_use\"",
            "final \"web_search_tool_result\"",
        ] {
            let count = seen.get(key).copied().unwrap_or(0);
            assert!(count >= 20, "only {count} of {key}; seen {seen:#?}");
        }
        assert!(!seen.contains_key("unparsed"), "seen {seen:#?}");
    }
}
