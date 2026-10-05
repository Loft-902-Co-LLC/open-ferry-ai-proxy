// Ported from CLIProxyAPI internal/translator/codex/interactions/interactions_codex_response.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Codex (OpenAI Responses) response → Gemini Interactions response, streamed
//! ([`CodexToInteractionsStream`]) or whole
//! ([`convert_codex_response_to_interactions_non_stream`]).
//!
//! The stream opens the interaction, then turns Codex's output items and
//! deltas into steps: `step.start`, `step.delta` and `step.stop` events, one
//! step open at a time. A completed or incomplete response, or `[DONE]`,
//! closes the interaction with its usage.
//!
//! Upstream's helpers that append a whole item to a response
//! (`appendCodex*ItemToInteractions`) and `codexStreamEventType` are not
//! ported: nothing calls them.
//!
//! Deviations from upstream:
//! - A chunk or body that isn't valid JSON reads as one with no fields.
//!   gjson reads what it can from it.
//! - Where upstream copies the client's raw JSON into a string (an ID, a
//!   model or a text that isn't a string), we write compact JSON. The values
//!   are the same JSON.
//! - A call's `arguments`, a string that begins as a JSON object but isn't
//!   one valid JSON value, is written `{}`. Upstream writes the text in as it
//!   is, so its response isn't valid JSON.
//! - A token count or creation time beyond `i64`'s range saturates, where
//!   Go's result depends on the CPU.
//! - Where a key is repeated in an object, the last one counts. gjson reads
//!   the first.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use crate::common::gemini_response::create_time;
use crate::common::sse::{push_event, push_frame};
use crate::go;
use crate::json::{exact, int_of, object, path, str_of};

/// The step kinds the stream opens.
const MODEL_OUTPUT: &str = "model_output";
const THOUGHT: &str = "thought";
const FUNCTION_CALL: &str = "function_call";

/// `codexToInteractionsStreamState`, with the translator's methods: turns a
/// Codex event stream into an Interactions one, a line at a time.
pub struct CodexToInteractionsStream {
    started: bool,
    completed: bool,
    done: bool,
    /// The step open, if any.
    step: Option<&'static str>,
    step_index: u64,
    next_step_index: u64,
    id: String,
    model: String,
    created_at: i64,
    has_output_text: bool,
    function_call_name: String,
    function_call_id: String,
}

impl CodexToInteractionsStream {
    /// A stream for `model`, with an ID made up from the clock until Codex
    /// gives one.
    pub fn new(model: &str) -> Self {
        Self {
            started: false,
            completed: false,
            done: false,
            step: None,
            step_index: 0,
            next_step_index: 0,
            id: format!("interaction_{}", unix_nanos()),
            model: model.to_owned(),
            created_at: 0,
            has_output_text: false,
            function_call_name: String::new(),
            function_call_id: String::new(),
        }
    }

    /// `ConvertCodexResponseToInteractions`: translates one line of Codex's
    /// stream, with or without its `data:` prefix, into the SSE frames to
    /// send, each ending in a blank line.
    pub fn translate_line(&mut self, line: &[u8]) -> String {
        let mut out = String::new();
        let mut payload = go::trim_space(line);
        if let Some(rest) = payload.strip_prefix(b"data:") {
            payload = go::trim_space(rest);
        }
        if payload == b"[DONE]" {
            self.step_stop(&mut out);
            if !self.completed {
                self.complete(&mut out, None);
            }
            self.finish(&mut out);
            return out;
        }
        if payload.is_empty() {
            return out;
        }
        let event = exact::from_slice(payload).unwrap_or(Value::Null);
        match str_of(event.get("type")).as_ref() {
            "response.created" => self.start(&mut out, event.get("response")),
            "response.output_item.added" => {
                self.start(&mut out, event.get("response"));
                let item = event.get("item");
                match str_of(item.and_then(|item| item.get("type"))).as_ref() {
                    "message" => self.ensure_step(&mut out, MODEL_OUTPUT, item),
                    "reasoning" => self.ensure_step(&mut out, THOUGHT, item),
                    "function_call" | "tool_call" => {
                        self.function_call_name =
                            str_of(item.and_then(|item| item.get("name"))).into_owned();
                        self.function_call_id = item.map(item_call_id).unwrap_or_default();
                        self.ensure_step(&mut out, FUNCTION_CALL, item);
                    }
                    _ => {}
                }
            }
            "response.output_text.delta" => {
                self.start(&mut out, event.get("response"));
                self.ensure_step(&mut out, MODEL_OUTPUT, None);
                self.text_delta(&mut out, &str_of(event.get("delta")));
                self.has_output_text = true;
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.start(&mut out, event.get("response"));
                self.ensure_step(&mut out, THOUGHT, None);
                self.thought_delta(&mut out, &str_of(event.get("delta")));
            }
            "response.function_call_arguments.delta" => {
                self.start(&mut out, event.get("response"));
                self.ensure_step(&mut out, FUNCTION_CALL, event.get("item"));
                self.arguments_delta(&mut out, &str_of(event.get("delta")));
            }
            "response.output_item.done" => self.item_done(&mut out, event.get("item")),
            "response.completed" | "response.incomplete" => {
                let response = event.get("response");
                self.start(&mut out, response);
                self.step_stop(&mut out);
                self.complete(&mut out, response);
                self.finish(&mut out);
            }
            _ => {}
        }
        out
    }

    /// `appendCodexInteractionsCreated`: opens the interaction, once, taking
    /// its ID, model and creation time from the response if it has them.
    fn start(&mut self, out: &mut String, response: Option<&Value>) {
        if self.started {
            return;
        }
        if let Some(response) = response {
            let id = str_of(response.get("id"));
            if !id.is_empty() {
                self.id = id.into_owned();
            }
            let model = str_of(response.get("model"));
            if !model.is_empty() {
                self.model = model.into_owned();
            }
            if let Some(created_at) = response.get("created_at") {
                self.created_at = int_of(created_at);
            }
        }
        let interaction = object([
            ("id", self.id.clone().into()),
            ("status", "in_progress".into()),
            ("object", "interaction".into()),
            ("model", self.model.clone().into()),
        ]);
        push_event(
            out,
            "interaction.created",
            &object([
                ("interaction", interaction),
                ("event_type", "interaction.created".into()),
            ]),
        );
        push_event(
            out,
            "interaction.status_update",
            &object([
                ("interaction_id", self.id.clone().into()),
                ("status", "in_progress".into()),
                ("event_type", "interaction.status_update".into()),
            ]),
        );
        self.started = true;
    }

    /// `appendCodexInteractionsCompleted`: closes the interaction, once,
    /// with the response's status and usage.
    fn complete(&mut self, out: &mut String, response: Option<&Value>) {
        if self.completed {
            return;
        }
        let now = unix_seconds();
        let created = if self.created_at > 0 {
            self.created_at
        } else {
            now
        };
        let mut interaction = Map::new();
        interaction.insert("id".into(), self.id.clone().into());
        interaction.insert("status".into(), "completed".into());
        interaction.insert("usage".into(), Value::Object(Map::new()));
        interaction.insert("created".into(), create_time(created).into());
        interaction.insert("updated".into(), create_time(now).into());
        interaction.insert("service_tier".into(), "standard".into());
        interaction.insert("object".into(), "interaction".into());
        interaction.insert("model".into(), self.model.clone().into());
        if let Some(response) = response {
            let status = str_of(response.get("status"));
            if !status.is_empty() {
                interaction.insert("status".into(), status.into_owned().into());
            }
            if let Some(usage) = response.get("usage") {
                interaction.insert("usage".into(), usage_object(usage, true));
            }
        }
        push_event(
            out,
            "interaction.completed",
            &object([
                ("interaction", Value::Object(interaction)),
                ("event_type", "interaction.completed".into()),
            ]),
        );
        self.completed = true;
    }

    /// `appendCodexInteractionsDone`: the closing `[DONE]`, once.
    fn finish(&mut self, out: &mut String) {
        if self.done {
            return;
        }
        push_frame(out, "done", "[DONE]");
        self.done = true;
    }

    /// `codexOutputItemDoneToInteractions`: an item's whole content, unless
    /// its deltas already gave it, then the step closes.
    fn item_done(&mut self, out: &mut String, item: Option<&Value>) {
        self.start(out, None);
        let field = |key: &str| item.and_then(|item| item.get(key));
        match str_of(field("type")).as_ref() {
            "message" => {
                if !self.has_output_text {
                    for content in values(field("content")) {
                        let text = content_text(content);
                        if !text.is_empty() {
                            self.ensure_step(out, MODEL_OUTPUT, item);
                            self.text_delta(out, text);
                        }
                    }
                }
                self.step_stop(out);
            }
            "reasoning" => {
                let text = item.map(reasoning_text).unwrap_or_default();
                if !text.is_empty() {
                    self.ensure_step(out, THOUGHT, item);
                    self.thought_delta(out, &text);
                }
                self.step_stop(out);
            }
            "function_call" | "tool_call" => {
                self.ensure_step(out, FUNCTION_CALL, item);
                self.arguments_delta(out, &str_of(field("arguments")));
                self.step_stop(out);
            }
            "image_generation_call" => {
                let result = str_of(field("result"));
                if !result.is_empty() {
                    self.ensure_step(out, MODEL_OUTPUT, item);
                    let content = object([
                        ("type", "image".into()),
                        (
                            "mime_type",
                            image_mime_type(&str_of(field("output_format"))).into(),
                        ),
                        ("data", result.into_owned().into()),
                    ]);
                    self.delta(
                        out,
                        object([("content", content), ("type", "content".into())]),
                    );
                }
                self.step_stop(out);
            }
            _ => {}
        }
    }

    /// `ensureCodexInteractionsStep`: opens a step of `kind` unless one is
    /// open, closing any other first.
    fn ensure_step(&mut self, out: &mut String, kind: &'static str, item: Option<&Value>) {
        if self.step == Some(kind) {
            return;
        }
        self.step_stop(out);
        self.step_start(out, kind, item);
    }

    /// `appendCodexInteractionsStepStart`. A function call step names its
    /// function and call, from the item or the item last added, with an ID
    /// made up from the clock if neither has one.
    fn step_start(&mut self, out: &mut String, kind: &'static str, item: Option<&Value>) {
        self.step_index = self.next_step_index;
        self.next_step_index += 1;
        self.step = Some(kind);
        let mut step = Map::new();
        step.insert("type".into(), kind.into());
        if kind == FUNCTION_CALL {
            let mut name = str_of(item.and_then(|item| item.get("name"))).into_owned();
            if name.is_empty() {
                name.clone_from(&self.function_call_name);
            }
            let mut call_id = item.map(item_call_id).unwrap_or_default();
            if call_id.is_empty() {
                call_id.clone_from(&self.function_call_id);
            }
            if call_id.is_empty() {
                call_id = format!("step_{}", unix_nanos());
            }
            step.insert("id".into(), call_id.clone().into());
            step.insert("call_id".into(), call_id.into());
            step.insert("name".into(), name.into());
            step.insert("arguments".into(), Value::Object(Map::new()));
        }
        push_event(
            out,
            "step.start",
            &object([
                ("index", self.step_index.into()),
                ("step", Value::Object(step)),
                ("event_type", "step.start".into()),
            ]),
        );
    }

    /// `appendCodexInteractionsStepStop`: closes the open step, if any.
    fn step_stop(&mut self, out: &mut String) {
        if self.step.take().is_none() {
            return;
        }
        push_event(
            out,
            "step.stop",
            &object([
                ("index", self.step_index.into()),
                ("event_type", "step.stop".into()),
            ]),
        );
    }

    fn text_delta(&self, out: &mut String, text: &str) {
        self.delta(
            out,
            object([("text", text.into()), ("type", "text".into())]),
        );
    }

    fn thought_delta(&self, out: &mut String, text: &str) {
        let content = object([("text", text.into()), ("type", "text".into())]);
        self.delta(
            out,
            object([("content", content), ("type", "thought_summary".into())]),
        );
    }

    fn arguments_delta(&self, out: &mut String, arguments: &str) {
        self.delta(
            out,
            object([
                ("arguments", arguments.into()),
                ("type", "arguments_delta".into()),
            ]),
        );
    }

    /// A `step.delta` event for the open step.
    fn delta(&self, out: &mut String, delta: Value) {
        push_event(
            out,
            "step.delta",
            &object([
                ("index", self.step_index.into()),
                ("delta", delta),
                ("event_type", "step.delta".into()),
            ]),
        );
    }
}

/// `ConvertCodexResponseToInteractionsNonStream`: converts Codex's final
/// event, or a bare response, into an Interactions response for
/// `model_name`.
pub fn convert_codex_response_to_interactions_non_stream(model_name: &str, event: &Value) -> Value {
    let response = event.get("response").unwrap_or(event);
    let mut out = Map::new();
    out.insert("id".into(), "".into());
    out.insert("object".into(), "interaction".into());
    out.insert("status".into(), "completed".into());
    out.insert("model".into(), "".into());
    out.insert("steps".into(), Value::Array(Vec::new()));
    let status = str_of(response.get("status"));
    if !status.is_empty() {
        out.insert("status".into(), status.into_owned().into());
    }
    let id = str_of(response.get("id"));
    let id = if id.is_empty() {
        format!("interaction_{}", unix_nanos())
    } else {
        id.into_owned()
    };
    out.insert("id".into(), id.into());
    let model = str_of(response.get("model"));
    let model = if model.is_empty() {
        model_name.to_owned()
    } else {
        model.into_owned()
    };
    out.insert("model".into(), model.into());
    let steps: Vec<Value> = values(response.get("output"))
        .filter_map(|item| match str_of(item.get("type")).as_ref() {
            "message" => message_step(item),
            "reasoning" => reasoning_step(item),
            "function_call" | "tool_call" => Some(function_call_step(item)),
            "image_generation_call" => image_step(item),
            _ => None,
        })
        .collect();
    if !steps.is_empty() {
        out.insert("steps".into(), Value::Array(steps));
    }
    if let Some(usage) = response.get("usage") {
        out.insert("usage".into(), usage_object(usage, false));
    }
    Value::Object(out)
}

/// `buildCodexMessageItemToInteractions`: the message's texts, if it has
/// any.
fn message_step(item: &Value) -> Option<Value> {
    let content: Vec<Value> = values(item.get("content"))
        .map(content_text)
        .filter(|text| !text.is_empty())
        .map(|text| object([("type", "text".into()), ("text", text.into())]))
        .collect();
    if content.is_empty() {
        return None;
    }
    Some(object([
        ("type", MODEL_OUTPUT.into()),
        ("content", Value::Array(content)),
    ]))
}

/// `buildCodexReasoningItemToInteractions`.
fn reasoning_step(item: &Value) -> Option<Value> {
    let text = reasoning_text(item);
    if text.is_empty() {
        return None;
    }
    let content = object([("type", "text".into()), ("text", text.into())]);
    Some(object([
        ("type", THOUGHT.into()),
        ("content", Value::Array(vec![content])),
    ]))
}

/// `buildCodexFunctionCallItemToInteractions`.
fn function_call_step(item: &Value) -> Value {
    let mut step = Map::new();
    step.insert("type".into(), FUNCTION_CALL.into());
    step.insert("name".into(), str_of(item.get("name")).into_owned().into());
    step.insert("arguments".into(), Value::Object(Map::new()));
    let call_id = item_call_id(item);
    if !call_id.is_empty() {
        step.insert("call_id".into(), call_id.into());
    }
    if let Some(arguments) = arguments_object(item.get("arguments")) {
        step.insert("arguments".into(), arguments);
    }
    Value::Object(step)
}

/// `buildCodexImageItemToInteractions`.
fn image_step(item: &Value) -> Option<Value> {
    let result = str_of(item.get("result"));
    if result.is_empty() {
        return None;
    }
    let content = object([
        ("type", "image".into()),
        (
            "mime_type",
            image_mime_type(&str_of(item.get("output_format"))).into(),
        ),
        ("data", result.into_owned().into()),
    ]);
    Some(object([
        ("type", MODEL_OUTPUT.into()),
        ("content", Value::Array(vec![content])),
    ]))
}

/// What gjson's `ForEach` visits that can hold fields: an array's items or
/// an object's values. It visits any other value itself, which has none.
fn values(value: Option<&Value>) -> Box<dyn Iterator<Item = &Value> + '_> {
    match value {
        Some(Value::Array(items)) => Box::new(items.iter()),
        Some(Value::Object(fields)) => Box::new(fields.values()),
        _ => Box::new(std::iter::empty()),
    }
}

/// `codexContentText`: a string `text`, else a string `content`.
fn content_text(content: &Value) -> &str {
    ["text", "content"]
        .into_iter()
        .find_map(|key| content.get(key).and_then(Value::as_str))
        .unwrap_or_default()
}

/// `codexReasoningText`: the item's content, a string or a list of parts,
/// else its summary, likewise. A part's text may be in `summary_text`.
fn reasoning_text(item: &Value) -> String {
    match item.get("content") {
        Some(Value::String(text)) => return text.clone(),
        Some(Value::Array(parts)) => {
            return join_lines(parts.iter().map(|part| {
                let text = content_text(part);
                if text.is_empty() {
                    str_of(part.get("summary_text"))
                } else {
                    text.into()
                }
            }));
        }
        _ => {}
    }
    match item.get("summary") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => join_lines(parts.iter().map(|part| content_text(part).into())),
        _ => String::new(),
    }
}

/// The texts that aren't empty, a line each.
fn join_lines<'t>(texts: impl Iterator<Item = std::borrow::Cow<'t, str>>) -> String {
    let mut joined = String::new();
    for text in texts.filter(|text| !text.is_empty()) {
        if !joined.is_empty() {
            joined.push('\n');
        }
        joined.push_str(&text);
    }
    joined
}

/// `codexItemCallID`: `call_id`, else `id`, trimmed.
fn item_call_id(item: &Value) -> String {
    let call_id = str_of(item.get("call_id"));
    if !call_id.trim().is_empty() {
        return call_id.trim().to_owned();
    }
    str_of(item.get("id")).trim().to_owned()
}

/// `codexArgumentsJSON`: an object, or a string holding one; any other
/// string gives `{}`, and anything else nothing.
fn arguments_object(arguments: Option<&Value>) -> Option<Value> {
    match arguments? {
        Value::String(text) => Some(match exact::from_str(text) {
            Ok(parsed @ Value::Object(_)) => parsed,
            _ => Value::Object(Map::new()),
        }),
        arguments @ Value::Object(_) => Some(arguments.clone()),
        _ => None,
    }
}

/// `setCodexInteractionsUsage`'s fields: the stream's totals, or the
/// response's counts. Codex's names come first, then Chat Completions'.
fn usage_object(usage: &Value, stream: bool) -> Value {
    let count = |key: &str| path(usage, key).map(int_of).unwrap_or(0);
    let either = |first: &str, second: &str| match count(first) {
        0 => count(second),
        found => found,
    };
    let input = either("input_tokens", "prompt_tokens");
    let output = either("output_tokens", "completion_tokens");
    let total = match count("total_tokens") {
        0 => input.wrapping_add(output),
        total => total,
    };
    let reasoning = either("output_tokens_details.reasoning_tokens", "reasoning_tokens");
    let cached = either("input_tokens_details.cached_tokens", "cached_tokens");
    let mut fields = Map::new();
    if stream {
        fields.insert("total_tokens".into(), total.into());
        fields.insert("total_input_tokens".into(), input.into());
        fields.insert(
            "input_tokens_by_modality".into(),
            Value::Array(vec![object([
                ("modality", "text".into()),
                ("tokens", input.into()),
            ])]),
        );
        fields.insert("total_cached_tokens".into(), cached.into());
        fields.insert("total_output_tokens".into(), output.into());
        fields.insert("total_tool_use_tokens".into(), 0.into());
        fields.insert("total_thought_tokens".into(), reasoning.into());
    } else {
        fields.insert("input_tokens".into(), input.into());
        fields.insert("output_tokens".into(), output.into());
        fields.insert("total_tokens".into(), total.into());
        if reasoning > 0 {
            fields.insert("reasoning_tokens".into(), reasoning.into());
        }
        if cached > 0 {
            fields.insert("cached_tokens".into(), cached.into());
        }
    }
    Value::Object(fields)
}

/// `mimeTypeFromCodexOutputFormat`.
fn image_mime_type(format: &str) -> String {
    if format.contains('/') {
        return format.to_owned();
    }
    match go::to_lower(format).as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        // `png`, nothing, and any other.
        _ => "image/png",
    }
    .to_owned()
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default()
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}
