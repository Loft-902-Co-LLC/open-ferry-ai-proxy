// Ported from CLIProxyAPI internal/translator/claude/interactions/interactions_claude_response.go
// (ConvertClaudeResponseToInteractions, ConvertClaudeResponseToInteractionsNonStream)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Claude Messages responses → Gemini Interactions responses.
//!
//! [`ClaudeToInteractionsStream`] turns each Claude SSE event into
//! Interactions events, one chunk per event: `interaction.created` and a
//! status update first, then a step for each content block (`thought` for
//! thinking, `function_call` for `tool_use`, `model_output` for the rest),
//! then `interaction.completed` with the usage. A `[DONE]` line gives a
//! `done` event.
//!
//! [`convert_claude_response_to_interactions_non_stream`] turns a whole Claude
//! message, or a whole Claude SSE stream, into one interaction.
//!
//! Deviations from upstream:
//! - An event or body that is not valid JSON is treated as having no fields.
//!   gjson reads what it can from malformed JSON.
//! - Where upstream copies Claude's JSON text into a string (a text, name or
//!   ID that is an object or an array), we write the same JSON compactly.
//! - Upstream leaves out a streamed `tool_use` block's `input` when its JSON
//!   text is exactly `{}`; we leave out any empty object, whatever its
//!   spacing, since we don't see the text.
//! - A token count beyond `i64` saturates; Go's conversion depends on the
//!   CPU.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::go;
use crate::json::{exact, int_of, raw, set_path, str_of};

/// The usage fields kept from Claude's events, merged as they arrive.
const USAGE_FIELDS: [&str; 5] = [
    "input_tokens",
    "output_tokens",
    "cache_read_input_tokens",
    "cache_creation_input_tokens",
    "thinking_tokens",
];

/// Translates a Claude Messages SSE stream into Interactions events, one
/// event at a time. Keep one per response: it tracks the interaction and the
/// step that is open.
pub struct ClaudeToInteractionsStream {
    model_name: String,
    id: String,
    model: String,
    created: bool,
    status_updated: bool,
    completed: bool,
    done: bool,
    /// The usage fields seen so far; `None` until an event has usage.
    usage: Option<Map<String, Value>>,
    /// How many steps have ended.
    step_index: i64,
    active_step_index: i64,
    active_step_open: bool,
    /// The step type of each content block, by Claude's block index.
    step_types: HashMap<i64, &'static str>,
    tool_names: HashMap<i64, String>,
    tool_ids: HashMap<i64, String>,
}

impl ClaudeToInteractionsStream {
    /// `model_name` is the model the request was for, used when the stream
    /// names none.
    pub fn new(model_name: &str) -> Self {
        Self {
            model_name: model_name.to_owned(),
            id: String::new(),
            model: model_name.to_owned(),
            created: false,
            status_updated: false,
            completed: false,
            done: false,
            usage: None,
            step_index: 0,
            active_step_index: 0,
            active_step_open: false,
            step_types: HashMap::new(),
            tool_names: HashMap::new(),
            tool_ids: HashMap::new(),
        }
    }

    /// Translates one Claude SSE line into Interactions SSE frames, each
    /// ending in a blank line. Only `data:` lines and `[DONE]` count.
    pub fn translate(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let Some(payload) = sse_payload(chunk) else {
            return out;
        };
        if payload.is_empty() {
            return out;
        }
        if payload == b"[DONE]" {
            self.done(&mut out);
            return out;
        }
        let root = exact::from_slice(payload).unwrap_or(Value::Null);
        match str_of(root.get("type")).as_ref() {
            "message_start" => {
                let message = root.get("message").unwrap_or(&Value::Null);
                self.id = first_non_empty(&[
                    &str_of(message.get("id")),
                    &self.id,
                    &format!("interaction_{}", unix_nanos()),
                ])
                .to_owned();
                self.model = first_non_empty(&[
                    &str_of(message.get("model")),
                    &self.model,
                    &self.model_name,
                ])
                .to_owned();
                self.merge_usage(message.get("usage"));
                let model = self.model.clone();
                self.created(&mut out, &model);
            }
            "content_block_start" => self.block_start(&mut out, &root),
            "content_block_delta" => self.block_delta(&mut out, &root),
            "content_block_stop" => {
                let index = root.get("index").map_or(0, int_of);
                self.step_stop(&mut out);
                self.step_types.remove(&index);
                self.tool_names.remove(&index);
                self.tool_ids.remove(&index);
            }
            "message_delta" => {
                self.merge_usage(root.get("usage"));
                self.step_stop(&mut out);
                self.completed(&mut out, &root);
            }
            "message_stop" => self.completed(&mut out, &root),
            "error" => {
                let model_name = self.model_name.clone();
                self.created(&mut out, &model_name);
                self.completed(&mut out, &root);
            }
            _ => {}
        }
        out
    }

    fn block_start(&mut self, out: &mut Vec<String>, root: &Value) {
        let model_name = self.model_name.clone();
        self.created(out, &model_name);
        self.step_stop(out);
        let index = root.get("index").map_or(0, int_of);
        let block = root.get("content_block").unwrap_or(&Value::Null);
        let step_type = block_step_type(&str_of(block.get("type")));
        self.step_types.insert(index, step_type);
        let mut step = json!({"type": step_type});
        if step_type == "function_call" {
            let name = str_of(block.get("name"));
            if !name.is_empty() {
                self.tool_names.insert(index, name.to_string());
            }
            let id = str_of(block.get("id"));
            if !id.is_empty() {
                self.tool_ids.insert(index, id.to_string());
            }
            set_function_call(&mut step, &name, &id);
        }
        self.step_start(out, step);
    }

    fn block_delta(&mut self, out: &mut Vec<String>, root: &Value) {
        let index = root.get("index").map_or(0, int_of);
        let delta = root.get("delta").unwrap_or(&Value::Null);
        let model_name = self.model_name.clone();
        match self.step_types.get(&index).copied() {
            None => {
                let step_type = delta_step_type(&str_of(delta.get("type")));
                self.created(out, &model_name);
                self.step_stop(out);
                self.step_start(out, json!({"type": step_type}));
                self.step_types.insert(index, step_type);
            }
            // The step counter is compared with Claude's block index, as
            // upstream does.
            Some(step_type) if !self.active_step_open || self.active_step_index != index => {
                self.created(out, &model_name);
                self.step_stop(out);
                let mut step = json!({"type": step_type});
                if step_type == "function_call" {
                    let name = self.tool_names.get(&index).map_or("", String::as_str);
                    let id = self.tool_ids.get(&index).map_or("", String::as_str);
                    set_function_call(&mut step, name, id);
                }
                self.step_start(out, step);
            }
            Some(_) => {}
        }
        match str_of(delta.get("type")).as_ref() {
            "text_delta" => self.text_delta(out, &str_of(delta.get("text")), false),
            "thinking_delta" => self.text_delta(out, &str_of(delta.get("thinking")), true),
            "input_json_delta" => {
                let arguments = str_of(delta.get("partial_json"));
                let payload = json!({
                    "index": self.active_step_index,
                    "delta": {"arguments": arguments, "type": "arguments_delta"},
                    "event_type": "step.delta",
                });
                push_frame(out, "step.delta", &payload);
            }
            _ => {}
        }
    }

    /// `interaction.created` and the first status update, once.
    fn created(&mut self, out: &mut Vec<String>, model_name: &str) {
        if self.created {
            return;
        }
        self.id = first_non_empty(&[&self.id, &format!("interaction_{}", unix_nanos())]).to_owned();
        let created = json!({
            "interaction": {
                "id": self.id,
                "status": "in_progress",
                "object": "interaction",
                "model": first_non_empty(&[&self.model, model_name]),
            },
            "event_type": "interaction.created",
        });
        push_frame(out, "interaction.created", &created);
        self.created = true;
        if !self.status_updated {
            let update = json!({
                "interaction_id": self.id,
                "status": "in_progress",
                "event_type": "interaction.status_update",
            });
            push_frame(out, "interaction.status_update", &update);
            self.status_updated = true;
        }
    }

    fn step_start(&mut self, out: &mut Vec<String>, step: Value) {
        self.active_step_index = self.step_index;
        self.active_step_open = true;
        let payload = json!({
            "index": self.active_step_index,
            "step": step,
            "event_type": "step.start",
        });
        push_frame(out, "step.start", &payload);
    }

    fn text_delta(&mut self, out: &mut Vec<String>, text: &str, thought: bool) {
        let delta = if thought {
            json!({"type": "thought_summary", "content": {"type": "text", "text": text}})
        } else {
            json!({"text": text, "type": "text"})
        };
        let payload = json!({
            "index": self.active_step_index,
            "delta": delta,
            "event_type": "step.delta",
        });
        push_frame(out, "step.delta", &payload);
    }

    fn step_stop(&mut self, out: &mut Vec<String>) {
        if !self.active_step_open {
            return;
        }
        let payload = json!({"index": self.active_step_index, "event_type": "step.stop"});
        push_frame(out, "step.stop", &payload);
        self.active_step_open = false;
        self.step_index = self.step_index.wrapping_add(1);
    }

    /// `interaction.completed`, once, with the merged usage or else the
    /// event's own.
    fn completed(&mut self, out: &mut Vec<String>, root: &Value) {
        if self.completed {
            return;
        }
        let model_name = self.model_name.clone();
        self.created(out, &model_name);
        let now = rfc3339_now();
        let mut completed = json!({
            "interaction": {
                "id": self.id,
                "status": "completed",
                "usage": {},
                "created": now,
                "updated": now,
                "service_tier": "standard",
                "object": "interaction",
                "model": first_non_empty(&[&self.model, &self.model_name]),
            },
            "event_type": "interaction.completed",
        });
        let merged = self.usage.clone().map(Value::Object);
        let usage = merged.as_ref().or_else(|| root.get("usage"));
        set_interactions_usage(&mut completed, "interaction.usage", usage);
        push_frame(out, "interaction.completed", &completed);
        self.completed = true;
    }

    fn done(&mut self, out: &mut Vec<String>) {
        if self.done {
            return;
        }
        out.push(frame("done", "[DONE]"));
        self.done = true;
    }

    fn merge_usage(&mut self, usage: Option<&Value>) {
        merge_usage(&mut self.usage, usage);
    }
}

/// Converts a whole Claude response into an interaction: a Claude message,
/// or else a Claude SSE stream, read line by line.
pub fn convert_claude_response_to_interactions_non_stream(model_name: &str, body: &[u8]) -> Value {
    match exact::from_slice(body) {
        Ok(root) if root.get("content").is_some() => message_to_interaction(model_name, &root),
        _ => stream_to_interaction(model_name, body),
    }
}

fn message_to_interaction(model_name: &str, root: &Value) -> Value {
    let mut out = json!({
        "id": first_non_empty(&[&str_of(root.get("id")), &format!("interaction_{}", unix_nanos())]),
        "object": "interaction",
        "status": "completed",
        "model": first_non_empty(&[&str_of(root.get("model")), model_name]),
        "steps": [],
    });
    let steps: Vec<Value> = for_each(root.get("content"))
        .into_iter()
        .filter_map(content_block_step)
        .collect();
    if !steps.is_empty() {
        out["steps"] = Value::Array(steps);
    }
    set_interactions_usage(&mut out, "usage", root.get("usage"));
    out
}

/// One content block of a Claude message as a step.
fn content_block_step(block: &Value) -> Option<Value> {
    match str_of(block.get("type")).as_ref() {
        "text" => Some(text_step("model_output", &str_of(block.get("text")))),
        "thinking" => Some(text_step("thought", &str_of(block.get("thinking")))),
        "tool_use" => Some(function_call_step(
            &str_of(block.get("name")),
            &str_of(block.get("id")),
            block.get("input"),
        )),
        _ => None,
    }
}

/// The blocks of a Claude stream being collected into an interaction.
#[derive(Default)]
struct Collected {
    usage: Option<Map<String, Value>>,
    step_types: HashMap<i64, &'static str>,
    tool_names: HashMap<i64, String>,
    tool_ids: HashMap<i64, String>,
    /// Each block's text, thinking or tool input so far.
    texts: HashMap<i64, String>,
}

fn stream_to_interaction(model_name: &str, body: &[u8]) -> Value {
    let mut out = json!({
        "id": format!("interaction_{}", unix_nanos()),
        "object": "interaction",
        "status": "completed",
        "model": model_name,
        "steps": [],
    });
    let mut collected = Collected::default();
    let mut steps = Vec::new();
    for line in body.split(|&byte| byte == b'\n') {
        let Some(payload) = go::trim_space(line).strip_prefix(b"data:") else {
            continue;
        };
        let payload = go::trim_space(payload);
        if payload == b"[DONE]" {
            continue;
        }
        let root = exact::from_slice(payload).unwrap_or(Value::Null);
        let index = root.get("index").map_or(0, int_of);
        match str_of(root.get("type")).as_ref() {
            "message_start" => {
                let message = root.get("message").unwrap_or(&Value::Null);
                let id = str_of(message.get("id"));
                if !id.is_empty() {
                    out["id"] = id.into();
                }
                let model = str_of(message.get("model"));
                if !model.is_empty() {
                    out["model"] = model.into();
                }
                merge_usage(&mut collected.usage, message.get("usage"));
            }
            "content_block_start" => {
                let block = root.get("content_block").unwrap_or(&Value::Null);
                let block_type = str_of(block.get("type"));
                collected
                    .step_types
                    .insert(index, block_step_type(&block_type));
                if block_type == "tool_use" {
                    collected
                        .tool_names
                        .insert(index, str_of(block.get("name")).into_owned());
                    collected
                        .tool_ids
                        .insert(index, str_of(block.get("id")).into_owned());
                    if let Some(input @ Value::Object(fields)) = block.get("input")
                        && !fields.is_empty()
                    {
                        collected.texts.insert(index, input.to_string());
                    }
                }
            }
            "content_block_delta" => {
                let delta = root.get("delta").unwrap_or(&Value::Null);
                let text = match str_of(delta.get("type")).as_ref() {
                    "text_delta" => delta.get("text"),
                    "thinking_delta" => delta.get("thinking"),
                    "input_json_delta" => delta.get("partial_json"),
                    _ => continue,
                };
                collected
                    .texts
                    .entry(index)
                    .or_default()
                    .push_str(&str_of(text));
            }
            "content_block_stop" => {
                let step_type = collected.step_types.remove(&index);
                let text = collected.texts.remove(&index).unwrap_or_default();
                let name = collected.tool_names.remove(&index).unwrap_or_default();
                let id = collected.tool_ids.remove(&index).unwrap_or_default();
                let step = match step_type {
                    Some("thought") => text_step("thought", &text),
                    Some("function_call") => {
                        let arguments = text.trim();
                        let arguments = (!arguments.is_empty() && raw::valid(arguments))
                            .then(|| exact::from_str(arguments).ok())
                            .flatten();
                        function_call_step(&name, &id, arguments.as_ref())
                    }
                    _ => text_step("model_output", &text),
                };
                steps.push(step);
            }
            "message_delta" => merge_usage(&mut collected.usage, root.get("usage")),
            _ => {}
        }
    }
    if !steps.is_empty() {
        out["steps"] = Value::Array(steps);
    }
    let usage = collected.usage.map(Value::Object);
    set_interactions_usage(&mut out, "usage", usage.as_ref());
    out
}

/// A step holding one text part.
fn text_step(step_type: &str, text: &str) -> Value {
    json!({"type": step_type, "content": [{"type": "text", "text": text}]})
}

/// A `function_call` step. `arguments` replaces the empty arguments if
/// given.
fn function_call_step(name: &str, id: &str, arguments: Option<&Value>) -> Value {
    let mut step = json!({"type": "function_call", "name": name, "arguments": {}});
    if !id.is_empty() {
        step["id"] = id.into();
        step["call_id"] = id.into();
    }
    if let Some(arguments) = arguments {
        step["arguments"] = arguments.clone();
    }
    step
}

/// Adds a `function_call` step's name, ID and empty arguments, as a stream's
/// `step.start` gives them.
fn set_function_call(step: &mut Value, name: &str, id: &str) {
    step["name"] = name.into();
    if !id.is_empty() {
        step["id"] = id.into();
        step["call_id"] = id.into();
    }
    step["arguments"] = json!({});
}

fn block_step_type(block_type: &str) -> &'static str {
    match block_type {
        "thinking" => "thought",
        "tool_use" => "function_call",
        _ => "model_output",
    }
}

fn delta_step_type(delta_type: &str) -> &'static str {
    match delta_type {
        "thinking_delta" => "thought",
        "input_json_delta" => "function_call",
        _ => "model_output",
    }
}

/// Merges an event's usage fields into `merged`. An event with usage, even
/// `null`, starts the merged usage.
fn merge_usage(merged: &mut Option<Map<String, Value>>, usage: Option<&Value>) {
    let Some(usage) = usage else {
        return;
    };
    let merged = merged.get_or_insert_with(Map::new);
    for key in USAGE_FIELDS {
        if let Some(value) = usage.get(key) {
            merged.insert(key.into(), value.clone());
        }
    }
}

/// `setInteractionsUsageFromClaude`: Claude usage as Interactions usage under
/// `prefix`.
fn set_interactions_usage(out: &mut Value, prefix: &str, usage: Option<&Value>) {
    let Some(usage) = usage else {
        return;
    };
    let tokens = |key: &str| usage.get(key).map_or(0, int_of);
    let input = usage.get("input_tokens").map(int_of);
    let output = usage.get("output_tokens").map(int_of);
    let mut set = |key: &str, value: i64| {
        set_path(out, &format!("{prefix}.{key}"), value.into());
    };
    if let Some(input) = input {
        set("input_tokens", input);
        set("total_input_tokens", input);
    }
    if let Some(output) = output {
        set("output_tokens", output);
        set("total_output_tokens", output);
    }
    if input.is_some() || output.is_some() {
        set(
            "total_tokens",
            input.unwrap_or(0).wrapping_add(output.unwrap_or(0)),
        );
    }
    let cache_read = tokens("cache_read_input_tokens");
    let cache_creation = tokens("cache_creation_input_tokens");
    if cache_read != 0 || cache_creation != 0 {
        let cached = cache_read.wrapping_add(cache_creation);
        set("cached_tokens", cached);
        set("total_cached_tokens", cached);
    }
    let thinking = tokens("thinking_tokens");
    if thinking != 0 {
        set("reasoning_tokens", thinking);
        set("total_thought_tokens", thinking);
    }
}

/// The payload of a `data:` line, or `[DONE]`; `None` for any other line.
fn sse_payload(chunk: &[u8]) -> Option<&[u8]> {
    let chunk = go::trim_space(chunk);
    if chunk == b"[DONE]" {
        return Some(chunk);
    }
    chunk.strip_prefix(b"data:").map(go::trim_space)
}

/// The first of `values` that isn't empty; `""` if they all are.
fn first_non_empty<'a>(values: &[&'a str]) -> &'a str {
    values
        .iter()
        .copied()
        .find(|value| !value.is_empty())
        .unwrap_or("")
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

/// One SSE frame, ending in a blank line.
fn frame(event: &str, data: &str) -> String {
    format!("event: {event}\ndata: {data}\n\n")
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

/// The time now as Go's `time.RFC3339` writes it in UTC, to the second.
fn rfc3339_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    rfc3339(seconds)
}

/// `seconds` since the Unix epoch as `YYYY-MM-DDThh:mm:ssZ`.
fn rfc3339(seconds: u64) -> String {
    let days = seconds / 86_400;
    let time = seconds % 86_400;
    // Howard Hinnant's civil_from_days, for days on or after 1970-01-01.
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3_600,
        time % 3_600 / 60,
        time % 60
    )
}

#[cfg(test)]
mod tests;
