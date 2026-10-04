//! Seeded random input for usage parsing: answers and stream lines of each
//! protocol the parsers read, with counts that add up and counts that
//! don't, buckets missing or under both their names, counts written as
//! gjson coerces them (floats, strings, booleans, null, values past `i64`),
//! usage that isn't an object, service tiers of each type and in each
//! place, and the framings the stream parsers read: `data:` with and
//! without a space, bare JSON, `[DONE]`, `event:` lines, blank lines and
//! lines cut short.

use serde_json::{Map, Value, json};

use super::{Generator, num};
use crate::cases::Case;

const PARSERS: &[&str] = &[
    "codex",
    "openai",
    "openai-stream",
    "claude",
    "claude-stream",
    "gemini",
    "gemini-stream",
];

/// Counts gjson reads other than as written, kept as text.
const ODD_COUNTS: &[&str] = &[
    "-1",
    "-5",
    "-0",
    "10.9",
    "1e1",
    "1.5e3",
    "2.0",
    "0.4",
    "9007199254740993",
    "9223372036854775807",
    "9223372036854775808",
    "-9223372036854775808",
    "18446744073709551616",
    "1e30",
    "-1e400",
];

/// Counts written as strings: gjson reads only a plain integer.
const STRING_COUNTS: &[&str] = &["10", "x", "-3", "", "1.5", " 4", "007"];

const SERVICE_TIERS: &[&str] = &[
    "default",
    "priority",
    "flex",
    " flex ",
    "scale",
    "",
    " ",
    "auto",
    "PRİORİTY",
];

/// Where a service tier may be written.
const TIER_PLACES: &[&str] = &["service_tier", "response", "interaction"];

/// `count` random cases for `usage/parse`, each depending only on `seed` and
/// its index.
pub fn parse_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed, index);
            let parser = generator.rng.pick(PARSERS);
            let body = generator.usage_body(parser);
            Case::new(format!("random-{seed}-{index}"), "", body)
                .with_options(json!({ "parser": parser }))
        })
        .collect()
}

impl Generator {
    /// A body or line for `parser`.
    fn usage_body(&mut self, parser: &str) -> String {
        match parser {
            "codex" => {
                let event = self.usage_codex_event();
                self.render(&event)
            }
            "openai" => {
                let answer = self.usage_openai_answer(false);
                self.render(&answer)
            }
            "openai-stream" => {
                let chunk = self.usage_openai_answer(true);
                self.usage_line(&chunk)
            }
            "claude" => {
                let message = self.usage_claude_message();
                self.render(&message)
            }
            "claude-stream" => {
                let event = self.usage_claude_event();
                self.usage_line(&event)
            }
            "gemini" => {
                let chunk = self.usage_gemini_chunk();
                self.render(&chunk)
            }
            _ => {
                let chunk = self.usage_gemini_chunk();
                self.usage_line(&chunk)
            }
        }
    }

    /// `payload` as a stream line, framed one of the ways the parsers read.
    fn usage_line(&mut self, payload: &Value) -> String {
        let text = self.render(payload);
        match self.rng.below(24) {
            0 => "data: [DONE]".to_owned(),
            1 => "[DONE]".to_owned(),
            2 => format!(
                "event: {}",
                self.rng.pick(&["message", "message_delta", "ping"])
            ),
            3 => self.rng.pick(&["", "   ", "data:", "data: "]).to_owned(),
            4 => format!("data: {}", cut(&text, self.rng.below(text.len().max(1)))),
            5 => format!("data:{text}"),
            6 => text,
            7 => format!("  data:   {text}  \r\n"),
            8 => format!("data: {text} trailing"),
            9 => format!("data: [{text}]"),
            _ => format!("data: {text}"),
        }
    }

    /// A count: mostly `n`, sometimes written another way.
    fn usage_count(&mut self, n: i64) -> Value {
        match self.rng.below(24) {
            0 => json!(self.rng.pick(STRING_COUNTS)),
            1 => self.rng.pick(&[json!(true), json!(false), Value::Null]),
            2 | 3 => num(self.rng.pick(ODD_COUNTS)),
            4 => json!(n.to_string()),
            _ => json!(n),
        }
    }

    /// A count from 0 to `max`, small more often than not.
    fn usage_amount(&mut self, max: u64) -> i64 {
        let max = if self.rng.chance(60) {
            max.min(64)
        } else {
            max
        };
        (self.rng.next() % (max + 1)) as i64
    }

    /// Usage that isn't an object.
    fn usage_not_object(&mut self) -> Value {
        self.rng.pick(&[
            Value::Null,
            json!("12"),
            json!(5),
            json!([{ "total_tokens": 3 }]),
            json!(true),
        ])
    }

    /// A service tier of any type.
    fn usage_tier(&mut self) -> Value {
        match self.rng.below(12) {
            0 => num(self.rng.pick(&["5", "1.50", "-0", "1e3"])),
            1 => self.rng.pick(&[json!(true), json!(false), Value::Null]),
            _ => json!(self.rng.pick(SERVICE_TIERS)),
        }
    }

    /// Adds a service tier to `answer` in one of the places it may be.
    fn usage_add_tier(&mut self, answer: &mut Map<String, Value>) {
        for _ in 0..self.rng.below(3) {
            let tier = self.usage_tier();
            match self.rng.pick(TIER_PLACES) {
                "service_tier" => {
                    answer.insert("service_tier".into(), tier);
                }
                place => {
                    let mut holder = Map::new();
                    holder.insert("id".into(), json!("resp_1"));
                    holder.insert("service_tier".into(), tier);
                    answer.insert(place.into(), Value::Object(holder));
                }
            }
        }
    }

    /// An OpenAI-style `usage`: Chat Completions' names, the Responses
    /// API's, or a mix.
    fn usage_openai_node(&mut self) -> Value {
        if self.rng.chance(8) {
            return self.usage_not_object();
        }
        let chat = self.rng.chance(50);
        let names = |chat: bool| {
            if chat {
                (
                    "prompt_tokens",
                    "completion_tokens",
                    "prompt_tokens_details",
                    "completion_tokens_details",
                )
            } else {
                (
                    "input_tokens",
                    "output_tokens",
                    "input_tokens_details",
                    "output_tokens_details",
                )
            }
        };
        let (input_name, output_name, input_details, output_details) = names(chat);
        let input = self.usage_amount(5000);
        let output = self.usage_amount(2000);
        let cached = if self.rng.chance(85) {
            self.usage_amount(input.max(0) as u64)
        } else {
            self.usage_amount(9000)
        };
        let cache_write = self.usage_amount(input.max(0) as u64);
        let reasoning = if self.rng.chance(85) {
            self.usage_amount(output.max(0) as u64)
        } else {
            self.usage_amount(3000)
        };
        let mut usage = Map::new();
        if self.rng.chance(85) {
            usage.insert(input_name.into(), self.usage_count(input));
        }
        if self.rng.chance(85) {
            usage.insert(output_name.into(), self.usage_count(output));
        }
        if self.rng.chance(10) {
            // Both names: the first in the parser's order wins.
            let (other_input, other_output, _, _) = names(!chat);
            let amount = self.usage_amount(5000);
            usage.insert(other_input.into(), self.usage_count(amount));
            let amount = self.usage_amount(2000);
            usage.insert(other_output.into(), self.usage_count(amount));
        }
        if self.rng.chance(60) {
            let mut details = Map::new();
            if self.rng.chance(80) {
                details.insert("cached_tokens".into(), self.usage_count(cached));
            }
            for name in ["cache_write_tokens", "cache_creation_tokens"] {
                if self.rng.chance(20) {
                    details.insert(name.into(), self.usage_count(cache_write));
                }
            }
            let details_name = if self.rng.chance(85) {
                input_details
            } else {
                names(!chat).2
            };
            usage.insert(details_name.into(), Value::Object(details));
        }
        if self.rng.chance(50) {
            let details_name = if self.rng.chance(85) {
                output_details
            } else {
                names(!chat).3
            };
            let reasoning = self.usage_count(reasoning);
            usage.insert(
                details_name.into(),
                json!({ "reasoning_tokens": reasoning }),
            );
        }
        if self.rng.chance(75) {
            let total = match self.rng.below(10) {
                0 => input + output + 7,
                1 => (input + output) / 2,
                2 => self.usage_amount(9000),
                _ => input + output,
            };
            usage.insert("total_tokens".into(), self.usage_count(total));
        }
        Value::Object(usage)
    }

    /// A Codex event: mostly `response.completed`.
    fn usage_codex_event(&mut self) -> Value {
        let kind = self.rng.pick(&[
            "response.completed",
            "response.completed",
            "response.done",
            "response.incomplete",
            "response.created",
        ]);
        let mut response = Map::new();
        response.insert("id".into(), json!("resp_1"));
        response.insert("object".into(), json!("response"));
        response.insert("status".into(), json!("completed"));
        if self.rng.chance(40) {
            let tier = self.usage_tier();
            response.insert("service_tier".into(), tier);
        }
        if self.rng.chance(85) {
            let usage = self.usage_openai_node();
            response.insert("usage".into(), usage);
        }
        let mut event = Map::new();
        event.insert("type".into(), json!(kind));
        if self.rng.chance(10) {
            self.usage_add_tier(&mut event);
        }
        if self.rng.chance(5) {
            let usage = self.usage_openai_node();
            event.insert("usage".into(), usage);
        }
        event.insert("response".into(), Value::Object(response));
        Value::Object(event)
    }

    /// A Chat Completions or Responses answer, or a stream chunk.
    fn usage_openai_answer(&mut self, chunk: bool) -> Value {
        let mut answer = Map::new();
        answer.insert("id".into(), json!("chatcmpl-1"));
        let object = if chunk {
            "chat.completion.chunk"
        } else {
            self.rng.pick(&["chat.completion", "response"])
        };
        answer.insert("object".into(), json!(object));
        answer.insert("model".into(), json!("gpt-5"));
        if self.rng.chance(40) {
            self.usage_add_tier(&mut answer);
        }
        if self.rng.chance(60) {
            let delta = if chunk { "delta" } else { "message" };
            answer.insert(
                "choices".into(),
                json!([{ "index": 0, delta: { "content": "usage" }, "finish_reason": null }]),
            );
        }
        if self.rng.chance(80) {
            let usage = self.usage_openai_node();
            answer.insert("usage".into(), usage);
        }
        Value::Object(answer)
    }

    /// A Claude `usage`.
    fn usage_claude_node(&mut self) -> Value {
        if self.rng.chance(8) {
            return self.usage_not_object();
        }
        let output = self.usage_amount(2000);
        let thinking = if self.rng.chance(80) {
            self.usage_amount(output.max(0) as u64)
        } else {
            self.usage_amount(4000)
        };
        let mut usage = Map::new();
        for (name, max, chance) in [
            ("input_tokens", 5000, 90),
            ("cache_creation_input_tokens", 20000, 40),
            ("cache_read_input_tokens", 400_000, 40),
        ] {
            if self.rng.chance(chance) {
                let amount = self.usage_amount(max);
                usage.insert(name.into(), self.usage_count(amount));
            }
        }
        if self.rng.chance(90) {
            usage.insert("output_tokens".into(), self.usage_count(output));
        }
        if self.rng.chance(30) {
            let name = self.rng.pick(&["thinking_tokens", "reasoning_tokens"]);
            let mut details = Map::new();
            details.insert(name.into(), self.usage_count(thinking));
            usage.insert("output_tokens_details".into(), Value::Object(details));
        }
        if self.rng.chance(15) {
            usage.insert("thinking_tokens".into(), self.usage_count(thinking));
        }
        if self.rng.chance(10) {
            usage.insert("service_tier".into(), json!("standard"));
        }
        Value::Object(usage)
    }

    /// A whole Claude message.
    fn usage_claude_message(&mut self) -> Value {
        let mut message = Map::new();
        message.insert("id".into(), json!("msg_1"));
        message.insert("type".into(), json!("message"));
        message.insert("role".into(), json!("assistant"));
        message.insert(
            "content".into(),
            json!([{ "type": "text", "text": "usage" }]),
        );
        if self.rng.chance(90) {
            let usage = self.usage_claude_node();
            message.insert("usage".into(), usage);
        }
        Value::Object(message)
    }

    /// A Claude stream event.
    fn usage_claude_event(&mut self) -> Value {
        match self.rng.below(10) {
            0..=3 => {
                let mut message = Map::new();
                message.insert("id".into(), json!("msg_1"));
                message.insert("model".into(), json!("claude-opus-5"));
                if self.rng.chance(90) {
                    let usage = self.usage_claude_node();
                    message.insert("usage".into(), usage);
                }
                json!({ "type": "message_start", "message": message })
            }
            4..=7 => {
                let mut event = Map::new();
                event.insert("type".into(), json!("message_delta"));
                event.insert("delta".into(), json!({ "stop_reason": "end_turn" }));
                if self.rng.chance(90) {
                    let usage = self.usage_claude_node();
                    event.insert("usage".into(), usage);
                }
                if self.rng.chance(10) {
                    let usage = self.usage_claude_node();
                    event.insert("message".into(), json!({ "usage": usage }));
                }
                Value::Object(event)
            }
            8 => json!({ "type": "ping" }),
            _ => json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": "usage" }
            }),
        }
    }

    /// A Gemini `usageMetadata`.
    fn usage_gemini_node(&mut self) -> Value {
        if self.rng.chance(8) {
            return self.usage_not_object();
        }
        let prompt = self.usage_amount(20000);
        let candidates = self.usage_amount(3000);
        let thoughts = self.usage_amount(2000);
        let tool_use = self.usage_amount(500);
        let mut node = Map::new();
        if self.rng.chance(85) {
            node.insert("promptTokenCount".into(), self.usage_count(prompt));
        }
        if self.rng.chance(80) {
            node.insert("candidatesTokenCount".into(), self.usage_count(candidates));
        }
        if self.rng.chance(40) {
            node.insert("thoughtsTokenCount".into(), self.usage_count(thoughts));
        }
        if self.rng.chance(30) {
            let cached = if self.rng.chance(85) {
                self.usage_amount(prompt.max(0) as u64)
            } else {
                self.usage_amount(30000)
            };
            node.insert("cachedContentTokenCount".into(), self.usage_count(cached));
        }
        if self.rng.chance(20) {
            let name = self
                .rng
                .pick(&["toolUsePromptTokenCount", "tool_use_prompt_token_count"]);
            node.insert(name.into(), self.usage_count(tool_use));
        }
        if self.rng.chance(70) {
            let total = match self.rng.below(10) {
                0 => prompt + candidates,
                1 => self.usage_amount(9000),
                2 => 0,
                _ => prompt + candidates + thoughts + tool_use,
            };
            node.insert("totalTokenCount".into(), self.usage_count(total));
        }
        if self.rng.chance(20) {
            node.insert(
                "promptTokensDetails".into(),
                json!([{ "modality": "TEXT", "tokenCount": prompt }]),
            );
        }
        Value::Object(node)
    }

    /// A Gemini answer or chunk, sometimes as Gemini CLI wraps it.
    fn usage_gemini_chunk(&mut self) -> Value {
        let mut chunk = Map::new();
        if self.rng.chance(70) {
            chunk.insert(
                "candidates".into(),
                json!([{ "content": { "role": "model", "parts": [{ "text": "usage" }] }, "index": 0 }]),
            );
        }
        if self.rng.chance(85) {
            let name = if self.rng.chance(85) {
                "usageMetadata"
            } else {
                "usage_metadata"
            };
            let node = self.usage_gemini_node();
            chunk.insert(name.into(), node);
        }
        if self.rng.chance(5) {
            let node = self.usage_gemini_node();
            chunk.insert("usage_metadata".into(), node);
        }
        chunk.insert("modelVersion".into(), json!("gemini-2.5-pro"));
        let chunk = Value::Object(chunk);
        if self.rng.chance(10) {
            json!({ "response": chunk })
        } else {
            chunk
        }
    }
}

/// `text` cut to at most `len` bytes, on a character boundary.
fn cut(text: &str, len: usize) -> &str {
    let mut end = len.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
