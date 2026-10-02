//! Live comparison through a running CLIProxyAPI.
//!
//! open-ferry has no server yet, so this tests the translators, not two proxies.
//! Each request is translated by upstream and by us, and both Codex bodies are
//! posted to the proxy's `/v1/responses`. Model output varies from run to run,
//! so we compare how the upstream API responded: HTTP status, the final event,
//! and which output items came back. The event streams that come back are then
//! run through both response translators, which must agree on them exactly.

use std::error::Error;
use std::io::{BufRead as _, BufReader, Read as _};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use ureq::Agent;

use crate::cases::Case;

/// Environment variable holding the proxy's client API key.
pub const API_KEY_VAR: &str = "OPEN_FERRY_PARITY_API_KEY";

#[derive(Clone, Copy)]
pub enum Expect {
    /// An assistant message with text.
    Text,
    /// A function call.
    FunctionCall,
    /// An assistant message whose text is a JSON object.
    JsonObject,
}

pub struct LiveCase {
    pub name: &'static str,
    /// What the case exercises, for the report.
    pub exercises: &'static str,
    pub request: String,
    pub expect: Expect,
}

/// A small set of realistic requests. Every one asks for low reasoning effort
/// to keep quota use down.
pub fn cases() -> Vec<LiveCase> {
    let effort = json!({ "type": "adaptive" });
    let low = json!({ "effort": "low" });
    let weather_tool = json!({
        "name": "get_weather",
        "description": "Get the current weather for a city.",
        "input_schema": {
            "type": "object",
            // Not in alphabetical order, so upstream reorders these keys and we don't.
            "properties": {
                "unit": { "type": "string", "enum": ["celsius", "fahrenheit"] },
                "city": { "type": "string" }
            },
            "required": ["city"]
        }
    });
    let round_trip = |call_id: &str| {
        json!({
            "thinking": effort,
            "output_config": low,
            "tools": [weather_tool],
            "messages": [
                { "role": "user", "content": "What's the weather in Zürich?" },
                { "role": "assistant", "content": [
                    { "type": "tool_use", "id": call_id, "name": "get_weather",
                      "input": { "city": "Zürich", "unit": "celsius" } }
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": call_id, "content": "18°C and sunny" }
                ]}
            ]
        })
    };
    // 46 bytes, then a 2-byte character across the 47-byte cut for long call IDs.
    let split_call_id = format!("toolu_{}é{}", "a".repeat(40), "b".repeat(30));

    vec![
        LiveCase {
            name: "text",
            exercises: "plain user message",
            request: json!({
                "thinking": effort,
                "output_config": low,
                "messages": [{ "role": "user", "content": "Reply with the single word: pong" }]
            })
            .to_string(),
            expect: Expect::Text,
        },
        LiveCase {
            name: "system-prompt",
            exercises: "system blocks to instructions",
            request: json!({
                "thinking": effort,
                "output_config": low,
                "system": [
                    { "type": "text", "text": "You are terse." },
                    { "type": "text", "text": "Answer in French." }
                ],
                "messages": [{ "role": "user", "content": "Say hello." }]
            })
            .to_string(),
            expect: Expect::Text,
        },
        LiveCase {
            name: "forced-tool",
            exercises: "tool_choice, parameter key order",
            request: json!({
                "thinking": effort,
                "output_config": low,
                "tools": [weather_tool],
                "tool_choice": { "type": "tool", "name": "get_weather" },
                "messages": [{ "role": "user", "content": "Weather in Oslo?" }]
            })
            .to_string(),
            expect: Expect::FunctionCall,
        },
        LiveCase {
            name: "tool-round-trip",
            exercises: "tool result, pretty arguments",
            // Pretty-printed, so upstream copies spaced-out tool input into
            // `arguments` and we write it compactly.
            request: serde_json::to_string_pretty(&round_trip("toolu_01A09q90qw90lq917835lq9"))
                .expect("serializable"),
            expect: Expect::Text,
        },
        LiveCase {
            name: "split-call-id",
            exercises: "long call ID cut mid-character",
            request: round_trip(&split_call_id).to_string(),
            expect: Expect::Text,
        },
        LiveCase {
            name: "long-tool-name",
            exercises: "tool name shortened to 64 bytes",
            request: {
                let mut tool = weather_tool.clone();
                let name = format!("get_weather_{}", "x".repeat(70));
                tool["name"] = name.clone().into();
                json!({
                    "thinking": effort,
                    "output_config": low,
                    "tools": [tool],
                    "tool_choice": { "type": "tool", "name": name },
                    "messages": [{ "role": "user", "content": "Weather in Lima?" }]
                })
                .to_string()
            },
            expect: Expect::FunctionCall,
        },
        LiveCase {
            name: "structured-output",
            exercises: "output_config.format",
            request: json!({
                "thinking": effort,
                "output_config": {
                    "effort": "low",
                    "format": {
                        "type": "json_schema",
                        "schema": {
                            "type": "object",
                            "properties": { "capital": { "type": "string" } },
                            "required": ["capital"],
                            "additionalProperties": false
                        }
                    }
                },
                "messages": [{ "role": "user", "content": "What is the capital of Peru?" }]
            })
            .to_string(),
            expect: Expect::JsonObject,
        },
    ]
}

impl LiveCase {
    pub fn to_case(&self, model: &str) -> Case {
        Case::new(self.name, model, self.request.clone())
    }
}

/// How the API responded to one body.
pub struct Reply {
    pub status: u16,
    /// The last `response.*` or `error` event, or the error body for a non-200.
    pub outcome: String,
    /// Output item types in order, except reasoning, which the model may skip.
    pub items: Vec<String>,
    pub call_names: Vec<String>,
    pub text: String,
    pub elapsed: Duration,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The event stream's lines as received.
    pub lines: Vec<String>,
}

impl Reply {
    /// The final event as upstream's executor gives it to the non-streaming
    /// translator: an empty `output` is filled with the items from
    /// `response.output_item.done` events, in `output_index` order.
    pub fn final_event(&self) -> Option<Value> {
        let mut items = Vec::new();
        let mut last = None;
        for line in &self.lines {
            let Some(event) = line
                .strip_prefix("data:")
                .and_then(|data| serde_json::from_str::<Value>(data.trim()).ok())
            else {
                continue;
            };
            match event["type"].as_str() {
                Some("response.output_item.done") => {
                    let index = event["output_index"].as_i64().unwrap_or(i64::MAX);
                    items.push((index, event["item"].clone()));
                }
                Some("response.completed" | "response.incomplete") => last = Some(event),
                _ => {}
            }
        }
        let mut event = last?;
        let output = &mut event["response"]["output"];
        if output.as_array().is_none_or(Vec::is_empty) && !items.is_empty() {
            items.sort_by_key(|(index, _)| *index);
            *output = items.into_iter().map(|(_, item)| item).collect();
        }
        Some(event)
    }

    pub fn meets(&self, expect: Expect) -> bool {
        let has = |kind: &str| self.items.iter().any(|item| item == kind);
        self.status == 200
            && self.outcome == "response.completed"
            && match expect {
                Expect::Text => has("message") && !self.text.trim().is_empty(),
                Expect::FunctionCall => has("function_call"),
                Expect::JsonObject => {
                    has("message")
                        && serde_json::from_str::<Value>(&self.text).is_ok_and(|v| v.is_object())
                }
            }
    }

    /// Whether two replies have the same shape. Text is not compared.
    pub fn same_shape(&self, other: &Self) -> bool {
        self.status == other.status
            && self.outcome == other.outcome
            && self.items == other.items
            && self.call_names == other.call_names
    }

    pub fn summary(&self) -> String {
        let mut items: Vec<String> = Vec::new();
        let mut names = self.call_names.iter();
        for item in &self.items {
            match (item.as_str(), names.next()) {
                ("function_call", Some(name)) => items.push(format!("function_call {name}")),
                (item, _) => items.push(item.to_owned()),
            }
        }
        format!("{} {} [{}]", self.status, self.outcome, items.join(", "))
    }

    pub fn to_json(&self) -> Value {
        json!({
            "status": self.status,
            "outcome": self.outcome,
            "items": self.items,
            "call_names": self.call_names,
            "text": self.text,
            "elapsed_ms": self.elapsed.as_millis() as u64,
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "lines": self.lines,
        })
    }
}

pub struct Client {
    agent: Agent,
    url: String,
    api_key: String,
}

impl Client {
    pub fn new(base_url: &str, api_key: String) -> Self {
        let agent: Agent = Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(300)))
            .build()
            .into();
        Self {
            agent,
            url: format!("{}/v1/responses", base_url.trim_end_matches('/')),
            api_key,
        }
    }

    /// Posts a Codex request body exactly as given and reads the event stream.
    pub fn send(&self, body: &[u8]) -> Result<Reply, Box<dyn Error>> {
        let started = Instant::now();
        let mut response = self
            .agent
            .post(&self.url)
            .header("Authorization", &format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .send(body)?;
        let mut reply = Reply {
            status: response.status().as_u16(),
            outcome: "(no final event)".into(),
            items: Vec::new(),
            call_names: Vec::new(),
            text: String::new(),
            elapsed: Duration::ZERO,
            input_tokens: 0,
            output_tokens: 0,
            lines: Vec::new(),
        };
        let reader = response.body_mut().as_reader();
        if reply.status != 200 {
            let mut body = String::new();
            reader.take(4096).read_to_string(&mut body)?;
            reply.outcome = body.trim().to_owned();
            reply.elapsed = started.elapsed();
            return Ok(reply);
        }

        for line in BufReader::new(reader).lines() {
            let line = line?;
            reply.lines.push(line.clone());
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<Value>(data.trim()) else {
                continue;
            };
            let kind = event["type"].as_str().unwrap_or_default();
            match kind {
                // Codex's final event may carry an empty `output`, so items are
                // read from these events instead.
                "response.output_item.done" => reply.record_item(&event["item"]),
                "response.completed" | "response.failed" | "response.incomplete" | "error" => {
                    reply.outcome = kind.to_owned();
                    let usage = &event["response"]["usage"];
                    reply.input_tokens = usage["input_tokens"].as_u64().unwrap_or(0);
                    reply.output_tokens = usage["output_tokens"].as_u64().unwrap_or(0);
                    if kind != "response.completed" {
                        let error = event.get("error").or(event["response"].get("error"));
                        if let Some(error) = error {
                            reply.outcome = format!("{kind} {error}");
                        }
                    }
                }
                _ => {}
            }
        }
        reply.elapsed = started.elapsed();
        Ok(reply)
    }
}

impl Reply {
    fn record_item(&mut self, item: &Value) {
        let kind = item["type"].as_str().unwrap_or("(untyped)");
        match kind {
            "reasoning" => return,
            "function_call" => self
                .call_names
                .push(item["name"].as_str().unwrap_or_default().to_owned()),
            "message" => {
                for part in item["content"].as_array().into_iter().flatten() {
                    self.text
                        .push_str(part["text"].as_str().unwrap_or_default());
                }
            }
            _ => {}
        }
        self.items.push(kind.to_owned());
    }
}
