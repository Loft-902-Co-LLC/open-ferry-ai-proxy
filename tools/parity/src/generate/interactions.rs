//! Seeded random input in the Gemini Interactions format, API revision
//! 2026-05-20, whose interactions are made of `steps`, for the Interactions
//! families' generators (each in a module below) to build on:
//! - [`request_cases`]: requests as an Interactions client sends them;
//! - [`event_cases`]: an interaction's event stream, as upstream's Gemini
//!   executor passes it to a translator, and the same interaction whole, as
//!   a non-streaming response.
//!
//! The families' generators may also call the functions under them, such as
//! `request`, `Interaction::random`, `Interaction::events` and
//! `Interaction::body`, with an [`Rng`] of their own.
//!
//! Like the other generators, it aims at the translators' corners: fields
//! under both their snake_case and camelCase names, values of the wrong
//! type, steps of every kind in any order, and text from the shared pools.
//! Model and agent names never contain "antigravity", whose translators and
//! branches are not ported.

pub mod chat;
pub mod claude;
pub mod codex;
pub mod gemini;
pub mod responses;

use serde_json::{Value, json};

use super::{EFFORTS, NUMBERS, Rng, SERVICE_TIERS, TEXTS, escape_text, num, to_object};
use crate::cases::Case;

/// Models, with the `models/` prefix some clients send. None names
/// antigravity.
const MODELS: &[&str] = &[
    "gemini-3-pro-preview",
    "gemini-3-flash-preview",
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-2.5-flash-lite",
    "models/gemini-2.5-flash",
    " Gemini-2.5-Pro ",
    "",
];

/// Agents, which a request names instead of a model.
const AGENTS: &[&str] = &[
    "deep-research-pro-preview-12-2025",
    "agents/custom-agent",
    "",
];

const TOOL_NAMES: &[&str] = &[
    "get_weather",
    "search",
    "lookup.v2",
    "run-shell",
    "mcp__server__tool",
    "名前",
    "a",
    "",
];

const MIME_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "application/pdf",
    "audio/wav",
    "video/mp4",
    "text/plain",
    "",
];

/// Content part types other than `text`, each with a MIME type and data or a
/// URI.
const MEDIA_TYPES: &[&str] = &["image", "document", "audio", "video"];

const ROLES: &[&str] = &["user", "model", "assistant", "system", "function", ""];

const STATUSES: &[&str] = &[
    "completed",
    "incomplete",
    "requires_action",
    "failed",
    "in_progress",
    "",
];

const FINISH_REASONS: &[&str] = &["stop", "length", "max_tokens", "tool_calls", "safety", ""];

const THINKING_LEVELS: &[&str] = &["minimal", "low", "medium", "high", "HIGH", " high ", ""];

const SUMMARIES: &[&str] = &["auto", "none", "detailed", "concise", "AUTO", ""];

const SIGNATURES: &[&str] = &[
    "c2lnbmF0dXJl",
    "Cq0BAXLI2nw=",
    "skip_thought_signature_validator",
    "",
];

const TOOL_CHOICES: &[&str] = &["auto", "none", "any", "required", "validated", "AUTO", ""];

/// Builds `count` request cases. Each case depends only on `seed` and its
/// index.
#[allow(
    dead_code,
    reason = "the Interactions families' generators call it as they land (WP4-A to WP4-E)"
)]
pub fn request_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut rng = rng(seed, index);
            let model = rng.pick(MODELS).to_owned();
            let request = request(&mut rng);
            let request = render(&mut rng, &request);
            Case::new(format!("interactions-{seed}-{index}"), model, request)
        })
        .collect()
}

/// Builds `count` interactions, each as a stream case and a non-streaming
/// case, with a request from [`request_cases`] as the client's.
#[allow(
    dead_code,
    reason = "the Interactions families' generators call it as they land (WP4-A to WP4-E)"
)]
pub fn event_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    (0..count as u64)
        .map(|index| {
            let mut rng = rng(seed, !index);
            let model = rng.pick(MODELS).to_owned();
            let request = request(&mut rng);
            let request = render(&mut rng, &request);
            let interaction = Interaction::random(&mut rng);
            let events = interaction.events(&mut rng);
            let body = interaction.body(&mut rng);
            let body = render(&mut rng, &body);
            let name = format!("interactions-{seed}-{index}");
            let stream = Case {
                model: model.clone(),
                ..Case::response(format!("{name}-stream"), request.clone(), events)
            };
            let body = Case {
                model,
                ..Case::response(format!("{name}-final"), request, vec![body])
            };
            (stream, body)
        })
        .unzip()
}

fn rng(seed: u64, index: u64) -> Rng {
    Rng(seed ^ 0x494E_5445_5241_4354 ^ index.wrapping_mul(0xA24B_AED4_963E_E407))
}

/// Serializes `value` compactly or pretty-printed, sometimes with every
/// non-ASCII character and `/` escaped.
fn render(rng: &mut Rng, value: &Value) -> String {
    let text = if rng.chance(30) {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    }
    .expect("a Value always serializes");
    if rng.chance(20) {
        escape_text(&text)
    } else {
        text
    }
}

fn text(rng: &mut Rng) -> String {
    rng.pick(TEXTS).to_owned()
}

/// A value of another type, where a string or an object belongs.
fn odd_value(rng: &mut Rng) -> Value {
    rng.pick(&[
        Value::Null,
        json!(1),
        json!(true),
        json!([]),
        json!({}),
        json!(""),
    ])
}

/// A token count: mostly small, sometimes any number literal.
fn token_count(rng: &mut Rng) -> Value {
    if rng.chance(80) {
        json!(rng.below(5000))
    } else {
        num(rng.pick(NUMBERS))
    }
}

/// An Interactions request body.
fn request(rng: &mut Rng) -> Value {
    let names: Vec<String> = (0..rng.below(4))
        .map(|_| rng.pick(TOOL_NAMES).to_owned())
        .collect();
    let mut fields: Vec<(&str, Value)> = Vec::new();
    match rng.below(10) {
        0 => fields.push(("agent", rng.pick(AGENTS).into())),
        1 => {}
        _ => fields.push(("model", rng.pick(MODELS).into())),
    }
    if rng.chance(40) {
        fields.push(("stream", json!(rng.chance(50))));
    }
    fields.push(("input", input(rng, &names)));
    if rng.chance(40) {
        let key = rng.pick(&["system_instruction", "systemInstruction"]);
        fields.push((key, system_instruction(rng)));
    }
    if !names.is_empty() || rng.chance(10) {
        fields.push(("tools", tools(rng, &names)));
    }
    if rng.chance(50) {
        let key = rng.pick(&["generation_config", "generationConfig"]);
        fields.push((key, generation_config(rng, &names)));
    }
    if rng.chance(20) {
        let reasoning = json!({ "effort": rng.pick(EFFORTS), "summary": rng.pick(SUMMARIES) });
        fields.push(("reasoning", reasoning));
    }
    if rng.chance(15) {
        fields.push(("response_format", response_format(rng)));
    }
    if rng.chance(15) {
        fields.push(("tool_choice", tool_choice(rng, &names)));
    }
    if rng.chance(10) {
        fields.push((
            "previous_interaction_id",
            json!("interaction_1700000000000000000"),
        ));
    }
    if rng.chance(10) {
        fields.push(if rng.chance(50) {
            ("environment_id", json!("env_1"))
        } else {
            ("environment", json!({ "id": "env_2" }))
        });
    }
    if rng.chance(10) {
        fields.push(("service_tier", rng.pick(SERVICE_TIERS).into()));
    }
    if rng.chance(30) {
        rng.shuffle(&mut fields);
    }
    to_object(fields)
}

/// A request's input: mostly a list of steps, sometimes turns, a string,
/// one content part, or a value of another type.
fn input(rng: &mut Rng, names: &[String]) -> Value {
    match rng.below(12) {
        0 => text(rng).into(),
        1 => content_part(rng),
        2 => odd_value(rng),
        3 | 4 => (0..1 + rng.below(4)).map(|_| turn(rng, names)).collect(),
        _ => steps(rng, names).into(),
    }
}

/// A turn: a role with its steps, Gemini-style parts, or content.
fn turn(rng: &mut Rng, names: &[String]) -> Value {
    let role = rng.pick(ROLES);
    match rng.below(3) {
        0 => json!({ "role": role, "steps": steps(rng, names) }),
        1 => json!({ "role": role, "parts": [{ "text": text(rng) }] }),
        _ => json!({ "role": role, "content": contents(rng) }),
    }
}

/// Steps as a client sends its conversation: user input, the model's output,
/// thoughts and calls, and results for the calls, in any order.
fn steps(rng: &mut Rng, names: &[String]) -> Vec<Value> {
    let mut steps = Vec::new();
    let mut calls: Vec<(String, String)> = Vec::new();
    for _ in 0..1 + rng.below(6) {
        let step = match rng.below(10) {
            0..=3 => json!({ "type": "user_input", "content": contents(rng) }),
            4 => json!({ "type": "model_output", "content": contents(rng) }),
            5 => thought_step(rng),
            6 | 7 => {
                let id = format!("call_{}", rng.below(1000));
                let name = names
                    .first()
                    .cloned()
                    .unwrap_or_else(|| rng.pick(TOOL_NAMES).to_owned());
                let name = if rng.chance(70) {
                    name
                } else {
                    rng.pick(TOOL_NAMES).to_owned()
                };
                calls.push((id.clone(), name.clone()));
                call_step(rng, &id, &name)
            }
            _ => result_step(rng, &calls),
        };
        steps.push(step);
    }
    steps
}

/// A step's content: a list of parts, or now and then a string.
fn contents(rng: &mut Rng) -> Value {
    if rng.chance(15) {
        return text(rng).into();
    }
    (0..1 + rng.below(3)).map(|_| content_part(rng)).collect()
}

fn content_part(rng: &mut Rng) -> Value {
    match rng.below(10) {
        0..=5 => json!({ "type": "text", "text": text(rng) }),
        6 => json!({ "type": "text", "text": odd_value(rng) }),
        7 | 8 => {
            let mut fields: Vec<(&str, Value)> = vec![
                ("type", rng.pick(MEDIA_TYPES).into()),
                ("mime_type", rng.pick(MIME_TYPES).into()),
            ];
            fields.push(if rng.chance(70) {
                ("data", json!("aGVsbG8="))
            } else {
                ("uri", json!("https://example.com/files/1"))
            });
            to_object(fields)
        }
        _ => json!({ "type": rng.pick(&["unknown", ""]), "text": text(rng) }),
    }
}

fn thought_step(rng: &mut Rng) -> Value {
    let mut fields: Vec<(&str, Value)> = vec![("type", json!("thought"))];
    if rng.chance(70) {
        let key = rng.pick(&["signature", "thought_signature", "thoughtSignature"]);
        fields.push((key, rng.pick(SIGNATURES).into()));
    }
    if rng.chance(70) {
        let key = rng.pick(&["content", "summary", "text"]);
        let value = if key == "text" {
            text(rng).into()
        } else {
            contents(rng)
        };
        fields.push((key, value));
    }
    to_object(fields)
}

fn call_step(rng: &mut Rng, id: &str, name: &str) -> Value {
    let mut fields: Vec<(&str, Value)> =
        vec![("type", json!("function_call")), ("name", name.into())];
    if rng.chance(90) {
        fields.push((rng.pick(&["call_id", "id"]), id.into()));
    }
    if rng.chance(90) {
        fields.push(("arguments", arguments(rng)));
    }
    if rng.chance(30) {
        let key = rng.pick(&["signature", "thought_signature"]);
        fields.push((key, rng.pick(SIGNATURES).into()));
    }
    to_object(fields)
}

/// A call's arguments: an object, its JSON text, or something else.
fn arguments(rng: &mut Rng) -> Value {
    let object = json!({ "city": text(rng), "count": num(rng.pick(NUMBERS)) });
    match rng.below(10) {
        0..=5 => object,
        6 | 7 => object.to_string().into(),
        8 => json!("{\"broken\":"),
        _ => odd_value(rng),
    }
}

/// A result for one of `calls`, or for a call never made.
fn result_step(rng: &mut Rng, calls: &[(String, String)]) -> Value {
    let (id, name) = if !calls.is_empty() && rng.chance(80) {
        calls[rng.below(calls.len())].clone()
    } else {
        ("call_orphan".to_owned(), rng.pick(TOOL_NAMES).to_owned())
    };
    let mut fields: Vec<(&str, Value)> = vec![("type", json!("function_result"))];
    if rng.chance(80) {
        fields.push(("name", name.into()));
    }
    if rng.chance(90) {
        fields.push((rng.pick(&["call_id", "id"]), id.into()));
    }
    let result = match rng.below(5) {
        0 | 1 => text(rng).into(),
        2 => json!({ "temperature": num(rng.pick(NUMBERS)), "note": text(rng) }),
        3 => json!([{ "type": "text", "text": text(rng) }]),
        _ => odd_value(rng),
    };
    fields.push(("result", result));
    if rng.chance(20) {
        fields.push(("is_error", json!(rng.chance(50))));
    }
    to_object(fields)
}

fn system_instruction(rng: &mut Rng) -> Value {
    match rng.below(4) {
        0 | 1 => text(rng).into(),
        2 => json!({ "parts": [{ "text": text(rng) }, { "text": text(rng) }] }),
        _ => json!({ "text": text(rng) }),
    }
}

/// A request's tools: functions declared one by one or in a declarations
/// list, and built-in tools.
fn tools(rng: &mut Rng, names: &[String]) -> Value {
    let declaration = |rng: &mut Rng, name: &str| {
        json!({
            "name": name,
            "description": text(rng),
            "parameters": {
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"],
            },
        })
    };
    let mut tools = Vec::new();
    if rng.chance(50) {
        for name in names {
            let mut tool = declaration(rng, name);
            if let Value::Object(fields) = &mut tool {
                fields.insert("type".into(), json!("function"));
            }
            tools.push(tool);
        }
    } else if !names.is_empty() {
        let key = rng.pick(&["function_declarations", "functionDeclarations"]);
        let declarations: Vec<Value> = names.iter().map(|name| declaration(rng, name)).collect();
        tools.push(json!({ key: declarations }));
    }
    if rng.chance(30) {
        tools.push(rng.pick(&[
            json!({ "type": "google_search" }),
            json!({ "type": "url_context" }),
            json!({ "type": "code_execution" }),
            json!({ "google_search": {} }),
        ]));
    }
    Value::Array(tools)
}

fn generation_config(rng: &mut Rng, names: &[String]) -> Value {
    let mut fields: Vec<(&str, Value)> = Vec::new();
    if rng.chance(40) {
        fields.push(("max_output_tokens", token_count(rng)));
    }
    if rng.chance(30) {
        fields.push((
            "temperature",
            num(rng.pick(&["0", "0.7", "1", "2.0", "-1"])),
        ));
    }
    if rng.chance(20) {
        fields.push(("top_p", num(rng.pick(&["0.9", "1", "0"]))));
    }
    if rng.chance(20) {
        fields.push(("stop_sequences", json!([text(rng)])));
    }
    if rng.chance(10) {
        fields.push(("seed", num(rng.pick(NUMBERS))));
    }
    if rng.chance(30) {
        fields.push(("thinking_level", rng.pick(THINKING_LEVELS).into()));
    }
    if rng.chance(20) {
        let key = rng.pick(&["thinking_summaries", "thinkingSummaries"]);
        fields.push((key, rng.pick(SUMMARIES).into()));
    }
    if rng.chance(20) {
        let key = rng.pick(&["thinking_config", "thinkingConfig"]);
        let config = json!({
            "thinking_budget": token_count(rng),
            "include_thoughts": rng.chance(50),
            "thinking_level": rng.pick(THINKING_LEVELS),
        });
        fields.push((key, config));
    }
    if rng.chance(15) {
        fields.push(("tool_choice", tool_choice(rng, names)));
    }
    if rng.chance(10) {
        fields.push(("response_modalities", json!(["TEXT"])));
    }
    if rng.chance(10) {
        fields.push((
            "response_schema",
            json!({ "type": "object", "properties": { "answer": { "type": "string" } } }),
        ));
    }
    to_object(fields)
}

fn response_format(rng: &mut Rng) -> Value {
    match rng.below(4) {
        0 => json!({ "type": "text" }),
        1 => json!({
            "type": "json_schema",
            "schema": { "type": "object", "properties": { "answer": { "type": "string" } } },
        }),
        2 => json!({ "type": "json_object" }),
        _ => odd_value(rng),
    }
}

fn tool_choice(rng: &mut Rng, names: &[String]) -> Value {
    match (rng.below(3), names.first()) {
        (0, Some(name)) => json!({ "type": "function", "name": name }),
        (1, Some(name)) => json!({ "allowed_tools": { "mode": "validated", "tools": [name] } }),
        _ => rng.pick(TOOL_CHOICES).into(),
    }
}

/// One step of an interaction, as an upstream answer holds it.
enum Step {
    Text(String),
    Thought {
        summary: String,
        signature: String,
    },
    Call {
        id: String,
        name: String,
        arguments: Value,
    },
}

/// How an interaction ends.
enum End {
    /// `interaction.completed`, with its status and finish reason.
    Completed {
        status: String,
        finish_reason: Option<String>,
    },
    /// `interaction.failed`, `response.failed` or an error alone.
    Failed { message: String, code: Value },
}

/// An interaction an upstream might answer with, to give as an event stream
/// ([`Self::events`]) or whole ([`Self::body`]).
struct Interaction {
    id: Option<String>,
    model: Option<String>,
    steps: Vec<Step>,
    usage: Option<Value>,
    end: End,
}

impl Interaction {
    fn random(rng: &mut Rng) -> Self {
        let steps = (0..rng.below(5))
            .map(|_| match rng.below(10) {
                0..=4 => Step::Text(text(rng)),
                5 | 6 => Step::Thought {
                    summary: text(rng),
                    signature: rng.pick(SIGNATURES).to_owned(),
                },
                _ => Step::Call {
                    id: format!("call_{}", rng.below(1000)),
                    name: rng.pick(TOOL_NAMES).to_owned(),
                    arguments: json!({ "city": text(rng) }),
                },
            })
            .collect();
        let usage = rng.chance(70).then(|| {
            let mut fields: Vec<(&str, Value)> = vec![
                ("total_input_tokens", token_count(rng)),
                ("total_output_tokens", token_count(rng)),
            ];
            for key in [
                "total_thought_tokens",
                "total_cached_tokens",
                "total_tokens",
            ] {
                if rng.chance(50) {
                    fields.push((key, token_count(rng)));
                }
            }
            to_object(fields)
        });
        let end = if rng.chance(85) {
            End::Completed {
                status: rng.pick(STATUSES).to_owned(),
                finish_reason: rng.chance(60).then(|| rng.pick(FINISH_REASONS).to_owned()),
            }
        } else {
            End::Failed {
                message: text(rng),
                code: rng.pick(&[json!(429), json!("RESOURCE_EXHAUSTED"), json!(500)]),
            }
        };
        Self {
            id: rng
                .chance(80)
                .then(|| format!("interaction_{}", rng.below(1000))),
            model: rng.chance(80).then(|| rng.pick(MODELS).to_owned()),
            steps,
            usage,
            end,
        }
    }

    /// The interaction as event lines, as upstream's Gemini executor passes
    /// them to a translator: mostly each event's JSON, sometimes as a
    /// `data:` line or a whole SSE frame, then sometimes `[DONE]`. Now and
    /// then an event is left out or repeated.
    fn events(&self, rng: &mut Rng) -> Vec<String> {
        let mut events = Vec::new();
        if rng.chance(90) {
            let mut interaction = serde_json::Map::new();
            if let Some(id) = &self.id {
                interaction.insert("id".into(), id.as_str().into());
            }
            if let Some(model) = &self.model {
                interaction.insert("model".into(), model.as_str().into());
            }
            if rng.chance(20) {
                interaction.insert("environment_id".into(), json!("env_1"));
            }
            events.push(json!({ "event_type": "interaction.created", "interaction": interaction }));
        }
        if rng.chance(10) {
            events.push(
                json!({ "event_type": "interaction.status_update", "status": "in_progress" }),
            );
        }
        for (index, step) in self.steps.iter().enumerate() {
            let start = match step {
                Step::Text(_) => json!({ "type": "model_output" }),
                Step::Thought { signature, .. } if rng.chance(30) => {
                    json!({ "type": "thought", "signature": signature })
                }
                Step::Thought { .. } => json!({ "type": "thought" }),
                Step::Call { id, name, .. } => {
                    json!({ "type": "function_call", rng.pick(&["id", "call_id"]): id, "name": name })
                }
            };
            events.push(json!({ "event_type": "step.start", "index": index, "step": start }));
            let deltas: Vec<Value> = match step {
                Step::Text(text) => pieces(rng, text)
                    .into_iter()
                    .map(|text| json!({ "type": "text", "text": text }))
                    .collect(),
                Step::Thought { summary, signature } => {
                    let mut deltas: Vec<Value> = pieces(rng, summary)
                        .into_iter()
                        .map(|text| {
                            json!({ "type": "thought_summary", "content": { "type": "text", "text": text } })
                        })
                        .collect();
                    deltas.push(json!({ "type": "thought_signature", "signature": signature }));
                    deltas
                }
                Step::Call { arguments, .. } => pieces(rng, &arguments.to_string())
                    .into_iter()
                    .map(|arguments| json!({ "type": "arguments_delta", "arguments": arguments }))
                    .collect(),
            };
            for delta in deltas {
                events.push(json!({ "event_type": "step.delta", "index": index, "delta": delta }));
            }
            events.push(json!({ "event_type": "step.stop", "index": index }));
        }
        match &self.end {
            End::Completed {
                status,
                finish_reason,
            } => {
                let mut interaction = serde_json::Map::new();
                if let Some(id) = &self.id {
                    interaction.insert("id".into(), id.as_str().into());
                }
                interaction.insert("status".into(), status.as_str().into());
                if let Some(reason) = finish_reason {
                    interaction.insert("finish_reason".into(), reason.as_str().into());
                }
                if let Some(usage) = &self.usage {
                    interaction.insert("usage".into(), usage.clone());
                }
                events.push(
                    json!({ "event_type": "interaction.completed", "interaction": interaction }),
                );
                if rng.chance(20)
                    && let Some(usage) = &self.usage
                {
                    events.push(
                        json!({ "event_type": "finish", "metadata": { "total_usage": usage } }),
                    );
                }
            }
            End::Failed { message, code } => {
                let error = json!({ "message": message, "code": code });
                events.push(match rng.below(3) {
                    0 => json!({ "event_type": "interaction.failed", "error": error }),
                    1 => json!({ "event_type": "interaction.failed", "interaction": { "error": error } }),
                    _ => json!({ "event_type": "response.failed", "error": error }),
                });
            }
        }
        if rng.chance(30) {
            events.push(json!({ "event_type": "done" }));
        }
        if !events.is_empty() && rng.chance(10) {
            let index = rng.below(events.len());
            if rng.chance(50) {
                events.remove(index);
            } else {
                let event = events[index].clone();
                events.insert(index, event);
            }
        }
        let mut lines: Vec<String> = events
            .iter()
            .map(|event| match rng.below(10) {
                0 => format!("data: {event}"),
                1 => format!(
                    "event: {}\ndata: {event}",
                    event["event_type"].as_str().unwrap_or("")
                ),
                _ => event.to_string(),
            })
            .collect();
        if rng.chance(40) {
            lines.push("[DONE]".to_owned());
        }
        lines
    }

    /// The interaction whole, as a non-streaming response: its steps,
    /// status and usage, sometimes under `interaction`, or only an error.
    fn body(&self, rng: &mut Rng) -> Value {
        let (status, finish_reason) = match &self.end {
            End::Failed { message, code } => {
                return json!({ "error": { "message": message, "code": code } });
            }
            End::Completed {
                status,
                finish_reason,
            } => (status, finish_reason),
        };
        let steps: Vec<Value> = self
            .steps
            .iter()
            .map(|step| match step {
                Step::Text(text) => {
                    json!({ "type": "model_output", "content": [{ "type": "text", "text": text }] })
                }
                Step::Thought { summary, signature } => json!({
                    "type": "thought",
                    rng.pick(&["signature", "thought_signature"]): signature,
                    "content": [{ "type": "text", "text": summary }],
                }),
                Step::Call {
                    id,
                    name,
                    arguments,
                } => json!({
                    "type": "function_call",
                    rng.pick(&["id", "call_id"]): id,
                    "name": name,
                    "arguments": arguments,
                }),
            })
            .collect();
        let mut fields: Vec<(&str, Value)> = Vec::new();
        if let Some(id) = &self.id {
            fields.push(("id", id.as_str().into()));
        }
        if let Some(model) = &self.model {
            fields.push(("model", model.as_str().into()));
        }
        fields.push(("object", json!("interaction")));
        fields.push(("status", status.as_str().into()));
        if let Some(reason) = finish_reason {
            fields.push(("finish_reason", reason.as_str().into()));
        }
        fields.push(("steps", steps.into()));
        if let Some(usage) = &self.usage {
            fields.push(("usage", usage.clone()));
        }
        let interaction = to_object(fields);
        if rng.chance(20) {
            json!({ "interaction": interaction })
        } else {
            interaction
        }
    }
}

/// `text` cut into one to three pieces at character boundaries.
fn pieces(rng: &mut Rng, text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut cuts: Vec<usize> = (0..rng.below(3))
        .map(|_| rng.below(chars.len() + 1))
        .collect();
    cuts.sort_unstable();
    let mut pieces = Vec::new();
    let mut start = 0;
    for cut in cuts.into_iter().chain([chars.len()]) {
        pieces.push(chars[start..cut].iter().collect());
        start = cut;
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cases_are_reproducible_and_valid_json() {
        let (first, again) = (request_cases(5, 200), request_cases(5, 200));
        assert_eq!(first.len(), 200);
        for (a, b) in first.iter().zip(&again) {
            assert_eq!((&a.request, &a.model), (&b.request, &b.model));
            serde_json::from_str::<Value>(&a.request).expect("a request is JSON");
        }
        let (streams, finals) = event_cases(5, 200);
        assert_eq!((streams.len(), finals.len()), (200, 200));
        assert_eq!(streams[7].events, event_cases(5, 200).0[7].events);
        for event in streams.iter().flat_map(|case| &case.events) {
            let payload = match event.strip_prefix("event: ") {
                Some(frame) => frame.split_once("\ndata: ").expect("a frame has data").1,
                None => event.strip_prefix("data: ").unwrap_or(event),
            };
            if payload != "[DONE]" {
                serde_json::from_str::<Value>(payload).expect("an event is JSON");
            }
        }
        for body in finals.iter().flat_map(|case| &case.events) {
            serde_json::from_str::<Value>(body).expect("a response is JSON");
        }
    }

    #[test]
    fn no_model_names_antigravity() {
        let (streams, finals) = event_cases(9, 100);
        let models = MODELS.iter().chain(AGENTS).map(|model| model.to_string());
        let cases = request_cases(9, 100)
            .into_iter()
            .chain(streams)
            .chain(finals);
        let texts =
            cases.flat_map(|case| [case.model, case.request].into_iter().chain(case.events));
        for text in models.chain(texts) {
            assert!(!text.to_lowercase().contains("antigravity"), "{text}");
        }
    }

    #[test]
    fn pieces_join_up() {
        let mut rng = rng(1, 2);
        for text in TEXTS {
            assert_eq!(pieces(&mut rng, text).concat(), *text);
        }
    }
}
