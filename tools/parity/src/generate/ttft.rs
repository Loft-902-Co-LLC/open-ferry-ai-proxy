//! Seeded random stream events for the first-token classifiers: each
//! protocol's event types, handled or not, with the fields they read
//! missing, empty, blank, null, zero, false, objects or arrays, or text;
//! lists that are single values or null; errors that are null; and the
//! framings the classifiers read: `data:` with and without a space, bare
//! JSON, `[DONE]`, `event:` lines before the data, blank lines and text
//! after the JSON.

use serde_json::{Map, Value, json};

use super::{Generator, num};
use crate::cases::Case;

const FORMATS: &[&str] = &["responses", "chat", "claude", "gemini"];

/// Responses API events the classifier reads, and some it doesn't.
const RESPONSES_TYPES: &[&str] = &[
    "response.reasoning_summary_text.delta",
    "response.reasoning.delta",
    "response.reasoning_text.delta",
    "response.output_text.delta",
    "response.text.delta",
    "response.function_call_arguments.delta",
    "response.custom_tool_call_input.delta",
    "response.code_interpreter_call_code.delta",
    "response.mcp_call_arguments.delta",
    "response.shell_call_command.delta",
    "response.refusal.delta",
    "response.audio.transcript.delta",
    "response.audio.delta",
    "response.image_generation_call.partial_image",
    "response.shell_call_command.added",
    "response.shell_call_command.done",
    "response.reasoning_summary_text.done",
    "response.reasoning_text.done",
    "response.output_text.done",
    "response.refusal.done",
    "response.function_call_arguments.done",
    "response.mcp_call_arguments.done",
    "response.custom_tool_call_input.done",
    "response.code_interpreter_call_code.done",
    "response.reasoning_summary_part.done",
    "response.content_part.done",
    "response.output_item.done",
    "response.output_item.done",
    "response.completed",
    "response.done",
    "response.incomplete",
    "response.failed",
    "error",
    "response.created",
    "response.in_progress",
    "response.output_item.added",
    "response.content_part.added",
    "response.reasoning_summary_part.added",
    "response.output_text.annotation.added",
    "response.queued",
    "Response.Completed",
    "response.output_text.delta ",
    "",
];

/// Fields the Responses classifier reads at the top of an event.
const RESPONSES_FIELDS: &[&str] = &[
    "delta",
    "data",
    "partial_image_b64",
    "command",
    "text",
    "refusal",
    "arguments",
    "input",
    "code",
];

/// Fields the Chat Completions classifier reads in a delta or message
/// (a message's `reasoning` it doesn't).
const CHAT_FIELDS: &[&str] = &["content", "reasoning_content", "reasoning", "refusal"];

const ITEM_TYPES: &[&str] = &[
    "function_call",
    "custom_tool_call",
    "message",
    "reasoning",
    "web_search_call",
    "Message",
    "",
];

/// Claude Messages events the classifier reads, and some it doesn't.
const CLAUDE_TYPES: &[&str] = &[
    "content_block_delta",
    "content_block_delta",
    "content_block_start",
    "message_delta",
    "message_stop",
    "error",
    "message_start",
    "ping",
    "content_block_stop",
    "message",
    "message",
    "completion",
    "",
];

const CLAUDE_DELTA_FIELDS: &[&str] = &["text", "thinking", "partial_json", "signature", "citation"];

const CLAUDE_BLOCK_TYPES: &[&str] = &["text", "thinking", "tool_use", "redacted_thinking", "image"];

/// Values put where a classifier reads text: the empty ones carry no token.
const VALUES: &[&str] = &[
    "\"\"",
    "\"\"",
    "\" \"",
    "\"Hi\"",
    "\"{\\\"a\\\":1}\"",
    "\"é\"",
    "null",
    "0",
    "-0",
    "1.50",
    "false",
    "true",
    "{}",
    "[]",
    "{\"text\":\"\"}",
    "[\"\"]",
];

const EVENT_NAMES: &[&str] = &[
    "message",
    "content_block_delta",
    "response.output_text.delta",
    "ping",
];

/// `count` random cases for `ttft/token-event`, each depending only on
/// `seed` and its index.
pub fn token_event_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed, index);
            let format = generator.rng.pick(FORMATS);
            let event = match format {
                "responses" => generator.ttft_responses_event(),
                "chat" => generator.ttft_chat_event(),
                "claude" => generator.ttft_claude_event(),
                _ => generator.ttft_gemini_event(),
            };
            let line = generator.ttft_line(&event);
            Case::new(format!("random-{seed}-{index}"), "", line)
                .with_options(json!({ "format": format }))
        })
        .collect()
}

impl Generator {
    /// `event` framed one of the ways the classifiers read.
    fn ttft_line(&mut self, event: &Value) -> String {
        let text = self.render(event);
        match self.rng.below(30) {
            0 => self
                .rng
                .pick(&[
                    "data: [DONE]",
                    "[DONE]",
                    "data:[DONE]",
                    " data:  [DONE] ",
                    "[DONE] x",
                ])
                .to_owned(),
            1 => self
                .rng
                .pick(&["", "  ", "data:", "data: ", "data:\t", "\n"])
                .to_owned(),
            2 | 3 => {
                let name = self.rng.pick(EVENT_NAMES);
                let newline = self.rng.pick(&["\n", "\r\n", "\n\n"]);
                format!("event: {name}{newline}data: {text}")
            }
            4 => format!("event: {}", self.rng.pick(EVENT_NAMES)),
            5 | 6 => format!("data:{text}"),
            7 | 8 => text,
            9 => format!("  data:   {text}  \r\n"),
            10 => format!("data: {text} trailing"),
            11 => format!("data: [{text}]"),
            12 => format!("data: data: {text}"),
            13 => format!("id: 1\ndata: {text}"),
            _ => format!("data: {text}"),
        }
    }

    /// A value where a classifier reads text.
    fn ttft_value(&mut self) -> Value {
        num(self.rng.pick(VALUES))
    }

    /// Sets `field` in `object` to a random value, most of the time.
    fn ttft_maybe(&mut self, object: &mut Map<String, Value>, field: &str, percent: u64) {
        if self.rng.chance(percent) {
            let value = self.ttft_value();
            object.insert(field.into(), value);
        }
    }

    /// A list of `items`, or now and then a single item or null.
    fn ttft_list(&mut self, items: Vec<Value>) -> Value {
        match self.rng.below(16) {
            0 => items.into_iter().next().unwrap_or(Value::Null),
            1 => Value::Null,
            2 => json!({}),
            _ => Value::Array(items),
        }
    }

    /// An event type: mostly one of `types`, sometimes not text.
    fn ttft_type(&mut self, types: &[&str]) -> Value {
        if self.rng.chance(4) {
            self.rng
                .pick(&[Value::Null, json!(1), json!(true), json!({})])
        } else {
            json!(self.rng.pick(types))
        }
    }

    /// A Responses API event.
    fn ttft_responses_event(&mut self) -> Value {
        let mut event = Map::new();
        if self.rng.chance(95) {
            let kind = self.ttft_type(RESPONSES_TYPES);
            event.insert("type".into(), kind);
        }
        if self.rng.chance(30) {
            event.insert("sequence_number".into(), json!(self.rng.below(50)));
        }
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if self.rng.chance(20) {
            event.insert("item_id".into(), json!("msg_1"));
            event.insert("output_index".into(), json!(0));
        }
        // The field the event's type reads, often, and others now and then.
        let field = self.rng.pick(RESPONSES_FIELDS);
        self.ttft_maybe(&mut event, field, 80);
        for field in RESPONSES_FIELDS {
            self.ttft_maybe(&mut event, field, 8);
        }
        if kind.ends_with("part.done") || self.rng.chance(5) {
            let part = self.ttft_part();
            event.insert("part".into(), part);
        }
        if kind == "response.output_item.done" || self.rng.chance(5) {
            let item = self.ttft_item();
            event.insert("item".into(), item);
        }
        if kind.starts_with("response.") && self.rng.chance(15) {
            event.insert(
                "response".into(),
                json!({ "id": "resp_1", "status": "in_progress", "output": [] }),
            );
        }
        Value::Object(event)
    }

    /// A content part, or now and then something else.
    fn ttft_part(&mut self) -> Value {
        if self.rng.chance(8) {
            return self.ttft_value();
        }
        let mut part = Map::new();
        let kind = self
            .rng
            .pick(&["output_text", "refusal", "summary_text", "reasoning_text"]);
        part.insert("type".into(), json!(kind));
        self.ttft_maybe(&mut part, "text", 70);
        self.ttft_maybe(&mut part, "refusal", 30);
        if self.rng.chance(20) {
            part.insert("annotations".into(), json!([]));
        }
        Value::Object(part)
    }

    /// An output item, or now and then something else.
    fn ttft_item(&mut self) -> Value {
        if self.rng.chance(6) {
            return self.ttft_value();
        }
        let mut item = Map::new();
        item.insert("id".into(), json!("item_1"));
        if self.rng.chance(95) {
            let kind = self.ttft_type(ITEM_TYPES);
            item.insert("type".into(), kind);
        }
        self.ttft_maybe(&mut item, "arguments", 50);
        self.ttft_maybe(&mut item, "input", 40);
        self.ttft_maybe(&mut item, "name", 30);
        if self.rng.chance(70) {
            let parts = (0..self.rng.below(4)).map(|_| self.ttft_part()).collect();
            let content = self.ttft_list(parts);
            item.insert("content".into(), content);
        }
        Value::Object(item)
    }

    /// A Chat Completions chunk or answer.
    fn ttft_chat_event(&mut self) -> Value {
        let mut chunk = Map::new();
        chunk.insert("id".into(), json!("chatcmpl-1"));
        let object = self.rng.pick(&["chat.completion.chunk", "chat.completion"]);
        chunk.insert("object".into(), json!(object));
        if self.rng.chance(6) {
            let error = match self.rng.below(4) {
                0 => Value::Null,
                1 => json!({ "message": null }),
                2 => json!({ "code": 429 }),
                _ => json!({ "message": "rate limited", "type": "rate_limit_error" }),
            };
            chunk.insert("error".into(), error);
        }
        if self.rng.chance(90) {
            let choices = (0..self.rng.below(3) + 1)
                .map(|index| self.ttft_choice(index))
                .collect();
            let choices = self.ttft_list(choices);
            chunk.insert("choices".into(), choices);
        }
        if self.rng.chance(15) {
            chunk.insert(
                "usage".into(),
                json!({ "prompt_tokens": 3, "completion_tokens": 1 }),
            );
        }
        Value::Object(chunk)
    }

    /// A choice with a delta, a message or both.
    fn ttft_choice(&mut self, index: usize) -> Value {
        let mut choice = Map::new();
        choice.insert("index".into(), json!(index));
        if self.rng.chance(75) {
            let delta = self.ttft_chat_message();
            choice.insert("delta".into(), delta);
        }
        if self.rng.chance(25) {
            let message = self.ttft_chat_message();
            choice.insert("message".into(), message);
        }
        if self.rng.chance(40) {
            let reason = match self.rng.below(5) {
                0 => json!("stop"),
                1 => json!("tool_calls"),
                _ => self.ttft_value(),
            };
            choice.insert("finish_reason".into(), reason);
        }
        Value::Object(choice)
    }

    /// A choice's delta or message.
    fn ttft_chat_message(&mut self) -> Value {
        if self.rng.chance(5) {
            return self.ttft_value();
        }
        let mut message = Map::new();
        if self.rng.chance(40) {
            message.insert("role".into(), json!("assistant"));
        }
        let field = self.rng.pick(CHAT_FIELDS);
        self.ttft_maybe(&mut message, field, 70);
        for field in CHAT_FIELDS {
            self.ttft_maybe(&mut message, field, 10);
        }
        if self.rng.chance(25) {
            let calls = (0..self.rng.below(3))
                .map(|_| self.ttft_tool_call())
                .collect();
            let calls = self.ttft_list(calls);
            message.insert("tool_calls".into(), calls);
        }
        Value::Object(message)
    }

    /// A tool call in a delta or message.
    fn ttft_tool_call(&mut self) -> Value {
        let mut call = Map::new();
        call.insert("index".into(), json!(0));
        if self.rng.chance(30) {
            call.insert("id".into(), json!("call_1"));
        }
        let kind = if self.rng.chance(80) {
            "function"
        } else {
            "custom"
        };
        call.insert("type".into(), json!(kind));
        let mut function = Map::new();
        self.ttft_maybe(&mut function, "name", 40);
        self.ttft_maybe(&mut function, "arguments", 60);
        self.ttft_maybe(&mut function, "input", 20);
        let function = if self.rng.chance(6) {
            self.ttft_value()
        } else {
            Value::Object(function)
        };
        call.insert(kind.into(), function);
        Value::Object(call)
    }

    /// A Claude Messages event or whole message.
    fn ttft_claude_event(&mut self) -> Value {
        let mut event = Map::new();
        let kind = if self.rng.chance(95) {
            let kind = self.ttft_type(CLAUDE_TYPES);
            event.insert("type".into(), kind.clone());
            kind.as_str().unwrap_or_default().to_owned()
        } else {
            String::new()
        };
        if kind.starts_with("content_block") {
            event.insert("index".into(), json!(self.rng.below(3)));
        }
        if kind == "content_block_delta" || self.rng.chance(8) {
            let mut delta = Map::new();
            let delta_kind = self.rng.pick(&[
                "text_delta",
                "thinking_delta",
                "input_json_delta",
                "signature_delta",
            ]);
            delta.insert("type".into(), json!(delta_kind));
            let field = self.rng.pick(CLAUDE_DELTA_FIELDS);
            self.ttft_maybe(&mut delta, field, 85);
            for field in CLAUDE_DELTA_FIELDS {
                self.ttft_maybe(&mut delta, field, 8);
            }
            self.ttft_maybe(&mut delta, "stop_reason", 5);
            event.insert("delta".into(), Value::Object(delta));
        }
        if kind == "content_block_start" || self.rng.chance(5) {
            let block = self.ttft_claude_block();
            event.insert("content_block".into(), block);
        }
        if kind == "message_delta" {
            let mut delta = Map::new();
            if self.rng.chance(85) {
                let reason = match self.rng.below(4) {
                    0 => json!("end_turn"),
                    1 => json!("tool_use"),
                    _ => self.ttft_value(),
                };
                delta.insert("stop_reason".into(), reason);
            }
            delta.insert("stop_sequence".into(), Value::Null);
            event.insert("delta".into(), Value::Object(delta));
            event.insert("usage".into(), json!({ "output_tokens": 5 }));
        }
        if kind == "message_start" {
            let content = self.ttft_claude_content();
            event.insert(
                "message".into(),
                json!({ "id": "msg_1", "role": "assistant", "content": content }),
            );
        }
        if kind == "error" {
            event.insert(
                "error".into(),
                json!({ "type": "overloaded_error", "message": "Overloaded" }),
            );
        }
        if kind.is_empty() || kind == "message" || kind == "completion" || self.rng.chance(5) {
            event.insert("id".into(), json!("msg_1"));
            event.insert("role".into(), json!("assistant"));
            if self.rng.chance(90) {
                let content = self.ttft_claude_content();
                event.insert("content".into(), content);
            }
        }
        Value::Object(event)
    }

    /// A message's content: mostly a list of blocks.
    fn ttft_claude_content(&mut self) -> Value {
        match self.rng.below(12) {
            0 => json!("Hi"),
            1 => self.ttft_value(),
            2 => self.ttft_claude_block(),
            _ => Value::Array(
                (0..self.rng.below(4))
                    .map(|_| self.ttft_claude_block())
                    .collect(),
            ),
        }
    }

    /// A content block.
    fn ttft_claude_block(&mut self) -> Value {
        if self.rng.chance(5) {
            return self.ttft_value();
        }
        let mut block = Map::new();
        let kind = self.rng.pick(CLAUDE_BLOCK_TYPES);
        if self.rng.chance(95) {
            block.insert("type".into(), json!(kind));
        }
        match kind {
            "text" => self.ttft_maybe(&mut block, "text", 85),
            "thinking" => {
                self.ttft_maybe(&mut block, "thinking", 85);
                self.ttft_maybe(&mut block, "signature", 40);
            }
            "tool_use" => {
                block.insert("id".into(), json!("toolu_1"));
                self.ttft_maybe(&mut block, "name", 85);
                block.insert("input".into(), json!({}));
            }
            _ => self.ttft_maybe(&mut block, "data", 50),
        }
        for field in ["text", "thinking", "name"] {
            self.ttft_maybe(&mut block, field, 5);
        }
        Value::Object(block)
    }

    /// A Gemini chunk, sometimes as Gemini CLI wraps it.
    fn ttft_gemini_event(&mut self) -> Value {
        let mut chunk = Map::new();
        if self.rng.chance(6) {
            let error = match self.rng.below(4) {
                0 => Value::Null,
                1 => json!({ "message": null }),
                2 => json!({ "code": 429, "status": "RESOURCE_EXHAUSTED" }),
                _ => json!({ "code": 500, "message": "internal" }),
            };
            chunk.insert("error".into(), error);
        }
        if self.rng.chance(90) {
            let candidates = (0..self.rng.below(3) + 1)
                .map(|index| self.ttft_candidate(index))
                .collect();
            let candidates = self.ttft_list(candidates);
            chunk.insert("candidates".into(), candidates);
        }
        if self.rng.chance(30) {
            chunk.insert(
                "usageMetadata".into(),
                json!({ "promptTokenCount": 3, "totalTokenCount": 3 }),
            );
        }
        chunk.insert("modelVersion".into(), json!("gemini-2.5-pro"));
        let chunk = Value::Object(chunk);
        if self.rng.chance(20) {
            let mut wrapper = Map::new();
            wrapper.insert("response".into(), chunk);
            if self.rng.chance(15) {
                wrapper.insert("candidates".into(), Value::Null);
            }
            if self.rng.chance(5) {
                wrapper.insert("traceId".into(), json!("t1"));
            }
            Value::Object(wrapper)
        } else {
            chunk
        }
    }

    /// A Gemini candidate.
    fn ttft_candidate(&mut self, index: usize) -> Value {
        let mut candidate = Map::new();
        candidate.insert("index".into(), json!(index));
        if self.rng.chance(85) {
            let parts = (0..self.rng.below(4))
                .map(|_| self.ttft_gemini_part())
                .collect();
            let parts = self.ttft_list(parts);
            let content = if self.rng.chance(5) {
                self.ttft_value()
            } else {
                json!({ "role": "model", "parts": parts })
            };
            candidate.insert("content".into(), content);
        }
        if self.rng.chance(30) {
            let reason = match self.rng.below(4) {
                0 => json!("STOP"),
                1 => json!("MAX_TOKENS"),
                _ => self.ttft_value(),
            };
            candidate.insert("finishReason".into(), reason);
        }
        Value::Object(candidate)
    }

    /// A Gemini part.
    fn ttft_gemini_part(&mut self) -> Value {
        if self.rng.chance(5) {
            return self.ttft_value();
        }
        let mut part = Map::new();
        match self.rng.below(6) {
            0 | 1 => self.ttft_maybe(&mut part, "text", 85),
            2 => {
                self.ttft_maybe(&mut part, "text", 60);
                let thought = if self.rng.chance(60) {
                    json!(true)
                } else {
                    self.ttft_value()
                };
                part.insert("thought".into(), thought);
            }
            3 => self.ttft_maybe(&mut part, "thoughtText", 85),
            4 => {
                let mut call = Map::new();
                self.ttft_maybe(&mut call, "name", 85);
                call.insert("args".into(), json!({}));
                let call = if self.rng.chance(8) {
                    self.ttft_value()
                } else {
                    Value::Object(call)
                };
                part.insert("functionCall".into(), call);
            }
            _ => {
                let mut data = Map::new();
                data.insert("mimeType".into(), json!("image/png"));
                self.ttft_maybe(&mut data, "data", 85);
                part.insert("inlineData".into(), Value::Object(data));
            }
        }
        if self.rng.chance(10) {
            part.insert("thoughtSignature".into(), json!("c2ln"));
        }
        Value::Object(part)
    }
}
