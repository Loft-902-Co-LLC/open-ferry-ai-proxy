// Ported from CLIProxyAPI internal/translator/interactions/claude/interactions_claude_request.go
// (ConvertClaudeRequestToInteractions, ConvertClaudeRequestToInteractionsWithCompat)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Claude Messages request → Gemini Interactions request.
//!
//! Messages become `input` steps: assistant messages become `model_output`
//! steps and the rest `user_input` steps, while `thinking`, `tool_use` and
//! `tool_result` blocks become `thought`, `function_call` and
//! `function_result` steps of their own. A tool result without a name gets
//! the name of the `tool_use` it answers. Mid-conversation `system` messages
//! become `<system-reminder>` user steps; one that arrives while tool calls
//! are waiting for their results is held back until the results are in.
//! `tool_result` blocks are put in the order of the calls they answer
//! ([`align_tool_results`]).
//!
//! Thinking settings become `thinking_level` or `thinking_budget`, and
//! `output_config.effort` overrides the level. Empty thinking blocks are
//! dropped, except by [`convert_claude_request_to_interactions_with_compat`].
//! Signatures are dropped: Interactions steps don't carry Claude's.
//!
//! An `image`, `document` or `container_upload` block without both a media
//! type and data is dropped. The rest of a user message is still sent, but a
//! user message left with nothing to send is refused with an
//! [`UnsupportedPartError`].
//!
//! Deviations from upstream:
//! - Where upstream copies the client's JSON text into a string (a `system`,
//!   text, thinking, name, ID, media type or data that is an object or an
//!   array), we write the same JSON compactly.

use std::borrow::Cow;
use std::collections::HashMap;

use serde_json::{Value, json};

use crate::common::claude::{align_tool_results, message_system_reminder_text};
use crate::common::parts::UserTurnDrops;
use crate::go;
use crate::json::{bool_of, object, path, set_path, str_of};
use crate::registry::UnsupportedPartError;

/// Converts a Claude Messages request body into an Interactions request for
/// `model_name`. Empty assistant thinking blocks are dropped. The request's
/// own `stream` wins over `stream`.
///
/// The error is set when a user message had only blocks Interactions can't
/// take; the body is still returned.
pub fn convert_claude_request_to_interactions(
    model_name: &str,
    request: &Value,
    stream: bool,
) -> (Value, Option<UnsupportedPartError>) {
    convert(model_name, request, stream, false)
}

/// [`convert_claude_request_to_interactions`] for compatibility endpoints,
/// which also keep empty assistant thinking blocks, as empty `thought` steps.
pub fn convert_claude_request_to_interactions_with_compat(
    model_name: &str,
    request: &Value,
    stream: bool,
) -> (Value, Option<UnsupportedPartError>) {
    convert(model_name, request, stream, true)
}

fn convert(
    model_name: &str,
    request: &Value,
    stream: bool,
    keep_empty_thinking: bool,
) -> (Value, Option<UnsupportedPartError>) {
    let mut out = json!({"model": "", "input": []});
    out["model"] = first_non_empty(&[model_name, &str_of(request.get("model"))]).into();
    match request.get("stream") {
        Some(value) => out["stream"] = bool_of(value).into(),
        None if stream => out["stream"] = true.into(),
        None => {}
    }

    let system = claude_text(request.get("system"));
    if !system.is_empty() {
        out["system_instruction"] = system.into();
    }

    for (from, to) in [
        ("max_tokens", "max_output_tokens"),
        ("temperature", "temperature"),
        ("top_p", "top_p"),
        ("stop_sequences", "stop_sequences"),
    ] {
        if let Some(value) = request.get(from) {
            set_path(&mut out, &format!("generation_config.{to}"), value.clone());
        }
    }
    apply_thinking(&mut out, request);
    if let Some(choice) = tool_choice(request.get("tool_choice")) {
        set_path(&mut out, "generation_config.tool_choice", choice);
    }

    let mut refusal = None;
    if let Some(Value::Array(messages)) = request.get("messages") {
        let (steps, err) = convert_messages(messages, keep_empty_thinking);
        out["input"] = Value::Array(steps);
        refusal = err;
    }

    if let Some(Value::Array(tools)) = request.get("tools") {
        let tools: Vec<Value> = tools.iter().filter_map(convert_tool).collect();
        if !tools.is_empty() {
            out["tools"] = Value::Array(tools);
        }
    }
    (out, refusal)
}

/// The first of `values` that isn't blank, as it is; `""` if they all are.
pub(super) fn first_non_empty<'a>(values: &[&'a str]) -> &'a str {
    values
        .iter()
        .copied()
        .find(|value| !value.trim().is_empty())
        .unwrap_or("")
}

/// Claude's `thinking` as a thinking level or budget, then
/// `output_config.effort`, if it is a string, as the level.
fn apply_thinking(out: &mut Value, request: &Value) {
    if let Some(thinking) = request.get("thinking") {
        match go::to_lower(str_of(thinking.get("type")).trim()).as_str() {
            "disabled" => {
                set_path(out, "generation_config.thinking_level", "none".into());
            }
            "enabled" => match thinking.get("budget_tokens") {
                Some(budget) => {
                    set_path(
                        out,
                        "generation_config.thinking_config.thinking_budget",
                        budget.clone(),
                    );
                }
                None => {
                    set_path(out, "generation_config.thinking_level", "high".into());
                }
            },
            "adaptive" => {
                set_path(out, "generation_config.thinking_level", "auto".into());
            }
            _ => {}
        }
    }
    if let Some(Value::String(effort)) = path(request, "output_config.effort") {
        set_path(
            out,
            "generation_config.thinking_level",
            go::to_lower(effort.trim()).into(),
        );
    }
}

/// Claude's `tool_choice` in Interactions' terms: `auto`, `required`, or a
/// named function. `none` and anything else give nothing.
fn tool_choice(choice: Option<&Value>) -> Option<Value> {
    let kind = match choice? {
        Value::String(text) => go::to_lower(text.trim()),
        choice @ (Value::Object(_) | Value::Array(_)) => {
            let kind = go::to_lower(str_of(choice.get("type")).trim());
            if kind == "tool" {
                let name = str_of(choice.get("name"));
                let name = name.trim();
                return (!name.is_empty()).then(|| json!({"type": "function", "name": name}));
            }
            kind
        }
        _ => return None,
    };
    match kind.as_str() {
        "auto" => Some("auto".into()),
        "any" | "required" => Some("required".into()),
        _ => None,
    }
}

/// A message's content, as `appendClaudeMessageToInteractions` reads it.
enum Content<'a> {
    Text(&'a str),
    Parts(&'a [Value]),
    Other,
}

impl<'a> From<Option<&'a Value>> for Content<'a> {
    fn from(content: Option<&'a Value>) -> Self {
        match content {
            Some(Value::String(text)) => Self::Text(text),
            Some(Value::Array(parts)) => Self::Parts(parts),
            _ => Self::Other,
        }
    }
}

/// Builds the `input` steps from Claude messages.
#[derive(Default)]
struct Steps {
    items: Vec<Value>,
    /// The IDs of the last message's `tool_use` blocks, whose results the
    /// next user message should hold.
    pending_tool_use_ids: Vec<String>,
    /// System reminders held back until the pending tool results are in.
    pending_reminders: Vec<Value>,
    tool_names_by_id: HashMap<String, String>,
    keep_empty_thinking: bool,
    /// The blocks each user message couldn't send.
    drops: UserTurnDrops,
}

fn convert_messages(
    messages: &[Value],
    keep_empty_thinking: bool,
) -> (Vec<Value>, Option<UnsupportedPartError>) {
    let mut steps = Steps {
        keep_empty_thinking,
        ..Steps::default()
    };
    for message in messages {
        let role = go::to_lower(str_of(message.get("role")).trim());
        let content = message.get("content");
        if role == "system" {
            if let Some(text) = message_system_reminder_text(content) {
                let step =
                    json!({"type": "user_input", "content": [{"type": "text", "text": text}]});
                if steps.pending_tool_use_ids.is_empty() {
                    steps.items.push(step);
                } else {
                    steps.pending_reminders.push(step);
                }
            }
            continue;
        }

        let aligned = match content {
            Some(Value::Array(parts)) if role == "user" => {
                Some(align_tool_results(parts, &steps.pending_tool_use_ids))
            }
            _ => None,
        };
        steps.pending_tool_use_ids.clear();
        let content = match &aligned {
            Some(parts) => Content::Parts(parts),
            None => Content::from(content),
        };
        let sendable = steps.append_message(&role, content);
        if role == "user" {
            steps.drops.end_turn(sendable);
        }
        steps.flush_reminders();
    }
    steps.flush_reminders();
    let refusal = steps.drops.err();
    (steps.items, refusal)
}

impl Steps {
    fn flush_reminders(&mut self) {
        self.items.append(&mut self.pending_reminders);
    }

    /// Ends the step being built from a message's text and media parts.
    fn flush_content(&mut self, step_type: &str, content: &mut Vec<Value>) {
        if content.is_empty() {
            return;
        }
        let content = std::mem::take(content);
        self.items.push(object([
            ("type", step_type.into()),
            ("content", content.into()),
        ]));
    }

    /// Adds the held-back reminders before a text or media part, ending the
    /// step being built first.
    fn flush_reminders_before_part(&mut self, step_type: &str, content: &mut Vec<Value>) {
        if !self.pending_reminders.is_empty() {
            self.flush_content(step_type, content);
            self.flush_reminders();
        }
    }

    /// Adds a message's steps, and says how many of its parts it sends.
    /// Text that is only whitespace is sent but isn't counted, nor are
    /// system reminders flushed beside it.
    fn append_message(&mut self, role: &str, content: Content<'_>) -> usize {
        let step_type = if role == "assistant" {
            "model_output"
        } else {
            "user_input"
        };
        let parts = match content {
            Content::Text(text) => {
                self.flush_reminders();
                self.items
                    .push(json!({"type": step_type, "content": [{"type": "text", "text": text}]}));
                return 1;
            }
            Content::Parts(parts) => parts,
            Content::Other => return 0,
        };

        let mut sendable = 0;
        let mut step_content = Vec::new();
        for part in parts {
            let part_type = go::to_lower(str_of(part.get("type")).trim());
            match part_type.as_str() {
                "text" => {
                    let text = str_of(part.get("text"));
                    if !text.is_empty() {
                        self.flush_reminders_before_part(step_type, &mut step_content);
                        if !text.trim().is_empty() {
                            sendable += 1;
                        }
                        step_content.push(json!({"type": "text", "text": text}));
                    }
                }
                "thinking" => {
                    self.flush_content(step_type, &mut step_content);
                    let text = str_of(part.get("thinking"));
                    if !text.is_empty() || self.keep_empty_thinking {
                        self.items.push(
                            json!({"type": "thought", "content": [{"type": "text", "text": text}]}),
                        );
                        sendable += 1;
                    }
                }
                "image" | "document" | "container_upload" => {
                    if let Some(media) = media_part(part, &part_type) {
                        self.flush_reminders_before_part(step_type, &mut step_content);
                        step_content.push(media);
                        sendable += 1;
                    } else if role == "user" {
                        self.drops.drop_part(&part_type);
                    }
                }
                "tool_use" => {
                    self.flush_content(step_type, &mut step_content);
                    let id = str_of(part.get("id"));
                    if !id.is_empty() {
                        self.pending_tool_use_ids.push(id.to_string());
                        let name = str_of(part.get("name"));
                        if !name.is_empty() {
                            self.tool_names_by_id
                                .insert(id.into_owned(), name.into_owned());
                        }
                    }
                    self.items.push(tool_use_step(part));
                    sendable += 1;
                }
                "tool_result" => {
                    self.flush_content(step_type, &mut step_content);
                    self.items
                        .push(tool_result_step(part, &self.tool_names_by_id));
                    sendable += 1;
                }
                _ => {}
            }
        }
        self.flush_content(step_type, &mut step_content);
        sendable
    }
}

/// An `image`, `document` or `container_upload` block as an Interactions
/// media part of the same type, if it has both a media type and data.
fn media_part(part: &Value, part_type: &str) -> Option<Value> {
    let source = part.get("source");
    let mut media_type = str_of(source.and_then(|source| source.get("media_type")));
    let mut data = str_of(source.and_then(|source| source.get("data")));
    if data.is_empty() {
        data = str_of(part.get("data"));
    }
    if media_type.is_empty() {
        media_type = str_of(part.get("mime_type"));
    }
    if media_type.is_empty() || data.is_empty() {
        return None;
    }
    Some(object([
        ("type", part_type.into()),
        ("mime_type", media_type.into()),
        ("data", data.into()),
    ]))
}

fn tool_use_step(part: &Value) -> Value {
    let mut step = object([
        ("type", "function_call".into()),
        ("name", str_of(part.get("name")).into()),
        ("arguments", json!({})),
    ]);
    let id = str_of(part.get("id"));
    if !id.is_empty() {
        step["id"] = id.into();
    }
    if let Some(input @ Value::Object(_)) = part.get("input") {
        step["arguments"] = input.clone();
    }
    step
}

fn tool_result_step(part: &Value, tool_names_by_id: &HashMap<String, String>) -> Value {
    let mut step = json!({"type": "function_result", "call_id": "", "result": ""});
    let id = str_of(part.get("tool_use_id"));
    if !id.is_empty() {
        step["call_id"] = id.as_ref().into();
    }
    let mut name = str_of(part.get("name"));
    if name.is_empty()
        && !id.is_empty()
        && let Some(known) = tool_names_by_id.get(id.as_ref())
    {
        name = Cow::Borrowed(known);
    }
    if !name.is_empty() {
        step["name"] = name.into();
    }
    if part.get("is_error").is_some_and(bool_of) {
        step["is_error"] = true.into();
    }
    match part.get("content") {
        None => {}
        Some(Value::String(text)) => step["result"] = text.as_str().into(),
        Some(Value::Array(items)) => {
            step["result"] = items.iter().map(tool_result_item).collect();
        }
        Some(other) => step["result"] = other.clone(),
    }
    step
}

/// One item of a tool result's content list. A text item with nothing but
/// `type`, `text` and `cache_control` is rewritten as plain text, and media
/// as an Interactions media part; anything else is kept as it is.
fn tool_result_item(item: &Value) -> Value {
    let item_type = str_of(item.get("type"));
    match item_type.as_ref() {
        "text" => {
            let plain = item.as_object().is_none_or(|fields| {
                fields
                    .keys()
                    .all(|key| matches!(key.as_str(), "type" | "text" | "cache_control"))
            });
            if plain {
                json!({"type": "text", "text": str_of(item.get("text"))})
            } else {
                item.clone()
            }
        }
        "image" | "document" | "container_upload" => {
            media_part(item, &item_type).unwrap_or_else(|| item.clone())
        }
        _ => item.clone(),
    }
}

/// A Claude tool as an Interactions function, if it has a name.
fn convert_tool(tool: &Value) -> Option<Value> {
    let name = str_of(tool.get("name"));
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let mut item = json!({"type": "function", "name": name, "parameters": {}});
    if let Some(description) = tool.get("description") {
        item["description"] = str_of(Some(description)).into();
    }
    if let Some(schema @ Value::Object(_)) = tool.get("input_schema") {
        item["parameters"] = schema.clone();
    }
    Some(item)
}

/// `claudeText`: a string as it is, an object's `text`, or a list's texts,
/// one per line.
fn claude_text(value: Option<&Value>) -> Cow<'_, str> {
    match value {
        None => Cow::Borrowed(""),
        Some(Value::String(text)) => Cow::Borrowed(text),
        Some(value) => {
            if let Some(text) = value.get("text") {
                return str_of(Some(text));
            }
            let Value::Array(items) = value else {
                return Cow::Borrowed("");
            };
            let texts: Vec<Cow<'_, str>> = items
                .iter()
                .map(|item| claude_text(Some(item)))
                .filter(|text| !text.is_empty())
                .collect();
            Cow::Owned(texts.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests;
