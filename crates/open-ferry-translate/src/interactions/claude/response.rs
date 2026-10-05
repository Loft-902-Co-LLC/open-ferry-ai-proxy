// Ported from CLIProxyAPI internal/translator/interactions/claude/interactions_claude_response.go
// (ConvertInteractionsResponseToClaude, ConvertInteractionsResponseToClaudeNonStream)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini Interactions responses → Claude Messages responses.
//!
//! [`InteractionsToClaudeStream`] turns each Interactions stream event into
//! Claude SSE events, one chunk per event, and
//! [`convert_interactions_response_to_claude_non_stream`] turns a whole
//! interaction into one Claude message. `thought` steps become thinking
//! blocks, `function_call` steps `tool_use` blocks and the rest text blocks;
//! a step's thought signature passes through. Usage is read from wherever
//! Interactions keeps it ([`interactions_usage`]), and cached tokens are
//! taken out of the input count when only a total is given.
//!
//! Deviations from upstream:
//! - An event or body that is not valid JSON is treated as having no fields.
//!   gjson reads what it can from malformed JSON.
//! - Where upstream copies Interactions' JSON text into a string (a text,
//!   name, ID or signature that is an object or an array, or a function
//!   call's `arguments` streamed in `partial_json`), we write the same JSON
//!   compactly.
//! - A token count beyond `i64` saturates; Go's conversion depends on the
//!   CPU.

use std::borrow::Cow;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use super::request::first_non_empty;
use crate::common::interactions_usage::interactions_usage;
use crate::go;
use crate::json::{exact, int_of, path, set_path, str_of};

/// Translates an Interactions event stream into Claude Messages SSE events,
/// one event at a time. Keep one per response: it tracks the message and the
/// content block that are open.
pub struct InteractionsToClaudeStream {
    model_name: String,
    id: String,
    model: String,
    started: bool,
    /// The type of the open content block: `text`, `thinking` or `tool_use`.
    block: Option<&'static str>,
    block_index: i64,
    saw_tool_call: bool,
    completed: bool,
    stopped: bool,
    done: bool,
    tool_names: HashMap<i64, String>,
    tool_ids: HashMap<i64, String>,
    tool_signatures: HashMap<i64, String>,
}

impl InteractionsToClaudeStream {
    /// `model_name` is the model the request was for, used when the stream
    /// names none.
    pub fn new(model_name: &str) -> Self {
        Self {
            model_name: model_name.to_owned(),
            id: String::new(),
            model: first_non_empty(&[model_name]).to_owned(),
            started: false,
            block: None,
            block_index: 0,
            saw_tool_call: false,
            completed: false,
            stopped: false,
            done: false,
            tool_names: HashMap::new(),
            tool_ids: HashMap::new(),
            tool_signatures: HashMap::new(),
        }
    }

    /// Translates one event (a line, or a whole SSE frame) into Claude SSE
    /// frames, each ending in two blank lines as upstream writes them.
    pub fn translate(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let payload = sse_payload(chunk);
        if payload.is_empty() {
            return out;
        }
        if go::trim_space(&payload) == b"[DONE]" {
            self.message_stop(&mut out);
            return out;
        }
        let Ok(root) = exact::from_slice(&payload) else {
            return out;
        };
        match str_of(root.get("event_type")).as_ref() {
            "interaction.created" => {
                let interaction = root.get("interaction");
                let field =
                    |key: &str| str_of(interaction.and_then(|interaction| interaction.get(key)));
                self.id = first_non_empty(&[&field("id"), &self.id]).to_owned();
                self.model =
                    first_non_empty(&[&field("model"), &self.model, &self.model_name]).to_owned();
                self.message_start(&mut out);
            }
            "step.start" => self.step_start(&mut out, &root),
            "step.delta" => self.step_delta(&mut out, &root),
            "step.stop" => self.block_stop(&mut out),
            "interaction.completed" | "finish" => self.message_delta(&mut out, &root),
            "response.failed" | "interaction.failed" => self.error(&mut out, &root),
            "done" => self.message_stop(&mut out),
            _ => {}
        }
        out
    }

    fn step_start(&mut self, out: &mut Vec<String>, root: &Value) {
        self.message_start(out);
        self.block_stop(out);
        let index = root.get("index").map_or(0, int_of);
        let step = root.get("step").unwrap_or(&Value::Null);
        match str_of(step.get("type")).as_ref() {
            "function_call" => {
                self.saw_tool_call = true;
                self.tool_names
                    .insert(index, str_of(step.get("name")).into_owned());
                self.tool_ids.insert(index, tool_id(step));
                self.tool_signatures.insert(index, signature(step));
                self.tool_block_start(out, index);
            }
            "thought" => self.block_start(out, "thinking"),
            _ => self.block_start(out, "text"),
        }
    }

    fn step_delta(&mut self, out: &mut Vec<String>, root: &Value) {
        let index = root.get("index").map_or(0, int_of);
        let delta = root.get("delta").unwrap_or(&Value::Null);
        match str_of(delta.get("type")).as_ref() {
            "thought_summary" => {
                self.message_start(out);
                self.block_start(out, "thinking");
                let text = first_non_empty(&[
                    &str_of(path(delta, "content.text")),
                    &str_of(delta.get("text")),
                ])
                .to_owned();
                self.content_delta(out, "thinking_delta", "thinking", &text);
            }
            "thought_signature" => {
                if self.block == Some("thinking") {
                    let signature = str_of(delta.get("signature"));
                    self.content_delta(out, "signature_delta", "signature", &signature);
                }
            }
            "arguments_delta" => {
                self.message_start(out);
                if self.block != Some("tool_use") {
                    self.block_stop(out);
                    if self.tool_names.get(&index).is_none_or(String::is_empty) {
                        self.tool_names
                            .insert(index, str_of(path(root, "step.name")).into_owned());
                    }
                    if self.tool_ids.get(&index).is_none_or(String::is_empty) {
                        self.tool_ids.insert(index, format!("toolu_{index}"));
                    }
                    self.tool_block_start(out, index);
                }
                let arguments = str_of(delta.get("arguments"));
                self.content_delta(out, "input_json_delta", "partial_json", &arguments);
            }
            _ => {
                self.message_start(out);
                self.block_start(out, "text");
                let text = str_of(delta.get("text"));
                self.content_delta(out, "text_delta", "text", &text);
            }
        }
    }

    fn message_start(&mut self, out: &mut Vec<String>) {
        if self.started {
            return;
        }
        let id = match first_non_empty(&[&self.id]) {
            "" => format!("msg_{}", unix_nanos()),
            id => id.to_owned(),
        };
        let message = json!({
            "type": "message_start",
            "message": {
                "id": id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": self.model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0},
            },
        });
        self.started = true;
        push_frame(out, "message_start", &message);
    }

    /// Opens a `thinking` or `text` block, unless one of that type is open.
    fn block_start(&mut self, out: &mut Vec<String>, block_type: &'static str) {
        if self.block == Some(block_type) {
            return;
        }
        self.block_stop(out);
        let content_block = if block_type == "thinking" {
            json!({"type": "thinking", "thinking": ""})
        } else {
            json!({"type": "text", "text": ""})
        };
        let start = json!({
            "type": "content_block_start",
            "index": self.block_index,
            "content_block": content_block,
        });
        self.block = Some(block_type);
        push_frame(out, "content_block_start", &start);
    }

    /// Opens a `tool_use` block for the function call of step `step_index`.
    fn tool_block_start(&mut self, out: &mut Vec<String>, step_index: i64) {
        self.block_stop(out);
        let id = self.tool_ids.get(&step_index).map_or("", String::as_str);
        let id = match first_non_empty(&[id]) {
            "" => format!("toolu_{step_index}"),
            id => id.to_owned(),
        };
        let name = self
            .tool_names
            .get(&step_index)
            .cloned()
            .unwrap_or_default();
        let mut content_block = json!({"type": "tool_use", "id": id, "name": name, "input": {}});
        if let Some(signature) = self
            .tool_signatures
            .get(&step_index)
            .filter(|signature| !signature.is_empty())
        {
            content_block["signature"] = signature.as_str().into();
        }
        let start = json!({
            "type": "content_block_start",
            "index": self.block_index,
            "content_block": content_block,
        });
        self.block = Some("tool_use");
        push_frame(out, "content_block_start", &start);
    }

    /// A delta for the open block. An empty one is dropped, except for a
    /// tool's input.
    fn content_delta(&mut self, out: &mut Vec<String>, delta_type: &str, field: &str, value: &str) {
        if value.is_empty() && delta_type != "input_json_delta" {
            return;
        }
        let mut delta = Map::new();
        delta.insert("type".into(), delta_type.into());
        delta.insert(field.into(), value.into());
        let event = json!({
            "type": "content_block_delta",
            "index": self.block_index,
            "delta": delta,
        });
        push_frame(out, "content_block_delta", &event);
    }

    fn block_stop(&mut self, out: &mut Vec<String>) {
        if self.block.is_none() {
            return;
        }
        let stop = json!({"type": "content_block_stop", "index": self.block_index});
        push_frame(out, "content_block_stop", &stop);
        self.block = None;
        self.block_index = self.block_index.wrapping_add(1);
    }

    /// The `message_delta` with the stop reason and usage, once.
    fn message_delta(&mut self, out: &mut Vec<String>, root: &Value) {
        if self.completed {
            return;
        }
        self.message_start(out);
        self.block_stop(out);
        let mut payload = json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": {"input_tokens": 0, "output_tokens": 0},
        });
        if self.saw_tool_call {
            set_path(&mut payload, "delta.stop_reason", "tool_use".into());
        }
        if stopped_at_max_tokens(root.get("interaction"), root) {
            set_path(&mut payload, "delta.stop_reason", "max_tokens".into());
        }
        set_claude_usage(&mut payload, interactions_usage(root));
        push_frame(out, "message_delta", &payload);
        self.completed = true;
    }

    /// Ends the message, once, with a `message_delta` first if none was sent.
    fn message_stop(&mut self, out: &mut Vec<String>) {
        if self.done {
            return;
        }
        self.block_stop(out);
        if !self.completed {
            self.message_delta(out, &Value::Null);
        }
        if !self.stopped {
            out.push(frame("message_stop", r#"{"type":"message_stop"}"#));
            self.stopped = true;
        }
        self.done = true;
    }

    fn error(&mut self, out: &mut Vec<String>, root: &Value) {
        self.block_stop(out);
        let error = root
            .get("error")
            .or_else(|| path(root, "interaction.error"))
            .unwrap_or(&Value::Null);
        let message = match str_of(error.get("message")) {
            message if message.is_empty() => Cow::Borrowed("upstream error occurred"),
            message => message,
        };
        let error_type = match str_of(error.get("type")) {
            error_type if error_type.is_empty() => Cow::Borrowed("api_error"),
            error_type => error_type,
        };
        let payload = json!({"type": "error", "error": {"type": error_type, "message": message}});
        push_frame(out, "error", &payload);
    }
}

/// Converts a whole Interactions response body into a Claude message.
pub fn convert_interactions_response_to_claude_non_stream(model_name: &str, body: &[u8]) -> Value {
    let root = exact::from_slice(body).unwrap_or(Value::Null);
    let interaction = root.get("interaction").unwrap_or(&root);
    let mut out = json!({
        "id": "",
        "type": "message",
        "role": "assistant",
        "model": "",
        "content": [],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 0, "output_tokens": 0},
    });
    out["id"] = first_non_empty(&[
        &str_of(interaction.get("id")),
        &str_of(root.get("id")),
        &format!("msg_{}", unix_nanos()),
    ])
    .into();
    out["model"] = first_non_empty(&[&str_of(interaction.get("model")), model_name]).into();

    let steps = interaction.get("steps").or_else(|| root.get("steps"));
    let mut saw_tool_call = false;
    let mut blocks = Vec::new();
    for step in for_each(steps) {
        match str_of(step.get("type")).as_ref() {
            "thought" => {
                for text in content_texts(step.get("content")) {
                    let mut block = json!({"type": "thinking", "thinking": text});
                    let signature = signature(step);
                    if !signature.is_empty() {
                        block["signature"] = signature.into();
                    }
                    blocks.push(block);
                }
            }
            "function_call" => {
                saw_tool_call = true;
                let mut block = json!({
                    "type": "tool_use",
                    "id": tool_id(step),
                    "name": str_of(step.get("name")),
                    "input": {},
                });
                let signature = signature(step);
                if !signature.is_empty() {
                    block["signature"] = signature.into();
                }
                if let Some(arguments @ Value::Object(_)) =
                    step.get("arguments").or_else(|| step.get("args"))
                {
                    block["input"] = arguments.clone();
                }
                blocks.push(block);
            }
            _ => {
                for text in content_texts(step.get("content")) {
                    blocks.push(json!({"type": "text", "text": text}));
                }
            }
        }
    }
    if !blocks.is_empty() {
        out["content"] = Value::Array(blocks);
    }
    if saw_tool_call {
        out["stop_reason"] = "tool_use".into();
    }
    if stopped_at_max_tokens(Some(interaction), &root) {
        out["stop_reason"] = "max_tokens".into();
    }
    set_claude_usage(&mut out, interactions_usage(&root));
    out
}

/// Whether the interaction stopped at its token limit: status `incomplete`,
/// or finish reason `length` or `max_tokens`, from the interaction or else
/// the event.
fn stopped_at_max_tokens(interaction: Option<&Value>, root: &Value) -> bool {
    let field = |key: &str| {
        let nested = str_of(interaction.and_then(|interaction| interaction.get(key)));
        first_non_empty(&[&nested, &str_of(root.get(key))]).to_owned()
    };
    let finish_reason = field("finish_reason");
    field("status") == "incomplete" || finish_reason == "length" || finish_reason == "max_tokens"
}

/// `setClaudeUsageFromInteractions`: Interactions usage as Claude's `usage`.
/// When only a total input count is given, the cached tokens are taken out
/// of it.
fn set_claude_usage(out: &mut Value, usage: Option<&Value>) {
    let Some(usage) = usage else {
        return;
    };
    let output = first_usage_int(usage, &["output_tokens", "total_output_tokens"]);
    let cached = first_usage_int(
        usage,
        &[
            "cache_read_input_tokens",
            "cache_read_tokens",
            "cached_tokens",
            "total_cached_tokens",
        ],
    )
    .filter(|&tokens| tokens > 0);
    let cache_write = first_usage_int(
        usage,
        &[
            "cache_creation_input_tokens",
            "cache_creation_tokens",
            "cache_write_tokens",
        ],
    )
    .filter(|&tokens| tokens > 0);
    let total_cache = cached.unwrap_or(0).wrapping_add(cache_write.unwrap_or(0));

    let input = match usage.get("input_tokens") {
        Some(tokens) => Some(int_of(tokens)),
        None => first_usage_int(usage, &["total_input_tokens", "prompt_tokens"]).map(|total| {
            if total >= total_cache {
                total.wrapping_sub(total_cache)
            } else {
                0
            }
        }),
    };
    for (key, tokens) in [
        ("input_tokens", input),
        ("output_tokens", output),
        ("cache_read_input_tokens", cached),
        ("cache_creation_input_tokens", cache_write),
    ] {
        if let Some(tokens) = tokens {
            set_path(out, &format!("usage.{key}"), tokens.into());
        }
    }
}

/// The first of `keys` that `usage` has, as an integer.
fn first_usage_int(usage: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|key| usage.get(*key)).map(int_of)
}

/// The payload of one stream event: the text after `data:`, or of every
/// `data:` line joined, or else the whole event.
fn sse_payload(chunk: &[u8]) -> Cow<'_, [u8]> {
    let trimmed = go::trim_space(chunk);
    if trimmed.is_empty() || trimmed == b"[DONE]" {
        return Cow::Borrowed(trimmed);
    }
    if let Some(rest) = trimmed.strip_prefix(b"data:") {
        return Cow::Borrowed(go::trim_space(rest));
    }
    let lines: Vec<&[u8]> = trimmed
        .split(|&byte| byte == b'\n')
        .filter_map(|line| go::trim_space(line).strip_prefix(b"data:"))
        .map(go::trim_space)
        .collect();
    if lines.is_empty() {
        Cow::Borrowed(trimmed)
    } else {
        Cow::Owned(lines.join(&b'\n'))
    }
}

/// gjson's `ForEach`: an array's items, an object's values, or anything else
/// once.
fn for_each(value: Option<&Value>) -> Vec<&Value> {
    match value {
        None => Vec::new(),
        Some(Value::Array(items)) => items.iter().collect(),
        Some(Value::Object(fields)) => fields.values().collect(),
        Some(other) => vec![other],
    }
}

/// A step's texts: its content if that is a string, or else each part's
/// `text` or `content.text` that isn't blank.
fn content_texts(content: Option<&Value>) -> Vec<String> {
    if let Some(Value::String(text)) = content {
        return vec![text.clone()];
    }
    for_each(content)
        .into_iter()
        .map(|part| {
            first_non_empty(&[
                &str_of(part.get("text")),
                &str_of(path(part, "content.text")),
            ])
            .to_owned()
        })
        .filter(|text| !text.is_empty())
        .collect()
}

fn tool_id(step: &Value) -> String {
    first_non_empty(&[
        &str_of(step.get("call_id")),
        &str_of(step.get("id")),
        &str_of(step.get("tool_use_id")),
        "toolu_interactions",
    ])
    .to_owned()
}

/// A step's thought signature, under any of the names Gemini uses.
fn signature(step: &Value) -> String {
    first_non_empty(&[
        &str_of(step.get("signature")),
        &str_of(step.get("thought_signature")),
        &str_of(step.get("thoughtSignature")),
        &str_of(path(step, "extra_content.google.thought_signature")),
    ])
    .to_owned()
}

/// One SSE frame as upstream writes it, ending in two blank lines.
fn frame(event: &str, data: &str) -> String {
    format!("event: {event}\ndata: {data}\n\n\n")
}

fn push_frame(out: &mut Vec<String>, event: &str, data: &Value) {
    out.push(frame(event, &data.to_string()));
}

fn unix_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests;
