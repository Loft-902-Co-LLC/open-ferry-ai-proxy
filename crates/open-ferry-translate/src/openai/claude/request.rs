// Ported from CLIProxyAPI internal/translator/openai/claude/openai_claude_request.go
// (ConvertClaudeRequestToOpenAI, ConvertClaudeRequestToOpenAIWithCompat) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Claude Messages request → OpenAI Chat Completions request.
//!
//! Claude's top-level `system` becomes a system message, and a mid-conversation
//! system message a user message holding a `<system-reminder>`. An
//! assistant's `tool_use` blocks become its tool calls, and each
//! `tool_result` a tool message placed right after the assistant message
//! that called it. Images a tool returned follow in a user message, since a
//! tool message can't carry them. An assistant's thinking is passed on as
//! `reasoning_content` only when its signature is one a GPT model can
//! replay, or always in compatibility mode. The thinking settings become a
//! `reasoning_effort`.
//!
//! Deviations from upstream:
//! - A tool's `input_schema` keeps the client's key order and number text;
//!   upstream round-trips it through a Go map, which sorts the keys and
//!   rewrites each number as a `float64`. A `properties` the schema gets is
//!   added last. A number beyond `f64`'s range is kept as written, where
//!   upstream fails to write the tool.
//! - Where upstream copies the client's JSON text into a string, we write the
//!   same JSON compactly: a tool call's `arguments`, a tool result's content
//!   that isn't text, and a non-string value read as text, such as a
//!   `stop_sequences` item or `user` that is an object.
//! - A `temperature` or `top_p` that isn't a finite number, such as `1e400`
//!   or the string `"NaN"`, is left out. Go writes it as `+Inf` or `NaN`,
//!   which isn't JSON.

use serde_json::{Map, Value, json};

use crate::common::claude::{
    align_tool_results, is_attribution_system_text, message_system_reminder_text,
};
use crate::common::openai_tools::align_openai_tool_call_messages;
use crate::go;
use crate::json::{float_of, int_of, object, path, str_of};
use crate::schema::{MAP_KEYWORDS, VALUE_KEYWORDS, has_unsupported_unicode_property_escape};
use crate::signature::{Provider, compatible_signature_for_provider};
use crate::thinking::{LEVEL_XHIGH, budget_to_level, thinking_text};

/// The text of a tool message whose result was only images.
pub(super) const TOOL_RESULT_IMAGE_PLACEHOLDER: &str =
    "[Tool returned image content; the images follow in the next user message.]";

/// The first part of the user message that carries a tool's images.
pub(super) const TOOL_RESULT_IMAGE_RELAY_NOTICE: &str =
    "Images returned by the preceding tool call(s):";

/// Converts a Claude Messages request body into a Chat Completions request
/// body for `model_name`. `stream` is whether the client asked to stream.
pub fn convert_claude_request_to_openai(model_name: &str, request: &Value, stream: bool) -> Value {
    convert(model_name, request, stream, false)
}

/// [`convert_claude_request_to_openai`] for compatibility endpoints, which
/// get every assistant thinking block back as `reasoning_content`, whatever
/// its signature.
pub fn convert_claude_request_to_openai_with_compat(
    model_name: &str,
    request: &Value,
    stream: bool,
) -> Value {
    convert(model_name, request, stream, true)
}

fn convert(model_name: &str, request: &Value, stream: bool, keep_thinking: bool) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), model_name.into());
    out.insert("messages".into(), json!([]));

    if let Some(max_tokens) = request.get("max_tokens") {
        out.insert("max_tokens".into(), int_of(max_tokens).into());
    }
    if let Some(temperature) = request.get("temperature") {
        if let Some(temperature) = float_of(temperature) {
            out.insert("temperature".into(), temperature);
        }
    } else if let Some(top_p) = request.get("top_p").and_then(float_of) {
        out.insert("top_p".into(), top_p);
    }
    if let Some(Value::Array(stops)) = request.get("stop_sequences")
        && !stops.is_empty()
    {
        let stops = stops.iter().map(|stop| str_of(Some(stop)).into()).collect();
        out.insert("stop".into(), Value::Array(stops));
    }
    out.insert("stream".into(), stream.into());
    if let Some(effort) = reasoning_effort(request) {
        out.insert("reasoning_effort".into(), effort.into());
    }

    let mut messages = Vec::new();
    if let Some(system) = system_message(request.get("system")) {
        messages.push(system);
    }
    if let Some(Value::Array(items)) = request.get("messages") {
        let mut converter = Messages {
            keep_thinking,
            out: messages,
            pending_tool_use_ids: Vec::new(),
            pending_reminders: Vec::new(),
            tool_names: std::collections::HashMap::new(),
        };
        for message in items {
            converter.push(message);
        }
        messages = converter.finish();
    }
    if !messages.is_empty() {
        let messages = align_openai_tool_call_messages(messages, []);
        out.insert("messages".into(), Value::Array(messages));
    }

    if let Some(Value::Array(tools)) = request.get("tools")
        && !tools.is_empty()
    {
        out.insert("tools".into(), tools.iter().map(convert_tool).collect());
    }

    if let Some(choice) = request
        .get("tool_choice")
        .filter(|choice| !choice.is_null())
    {
        out.insert("tool_choice".into(), convert_tool_choice(choice));
        if choice.get("disable_parallel_tool_use") == Some(&Value::Bool(true)) {
            out.insert("parallel_tool_calls".into(), false.into());
        }
    }

    if let Some(user) = request.get("user") {
        out.insert("user".into(), str_of(Some(user)).into());
    }

    Value::Object(out)
}

/// The `reasoning_effort` for Claude's `thinking` settings, if they call for
/// one.
fn reasoning_effort(request: &Value) -> Option<String> {
    let thinking = request
        .get("thinking")
        .filter(|thinking| thinking.is_object())?;
    let kind = thinking.get("type")?;
    let explicit_effort = || match path(request, "output_config.effort") {
        Some(Value::String(effort)) => Some(go::to_lower(effort.trim())),
        _ => None,
    };
    match &*str_of(Some(kind)) {
        "enabled" => {
            if let Some(budget) = thinking.get("budget_tokens") {
                budget_to_level(int_of(budget))
                    .filter(|effort| !effort.is_empty())
                    .map(str::to_owned)
            } else if let Some(effort) = explicit_effort().filter(|effort| !effort.is_empty()) {
                // A client may pair manual thinking with an explicit effort
                // and no token budget.
                Some(effort)
            } else {
                budget_to_level(-1).map(str::to_owned)
            }
        }
        "adaptive" | "auto" => Some(
            explicit_effort()
                .filter(|effort| !effort.is_empty())
                .unwrap_or_else(|| LEVEL_XHIGH.to_owned()),
        ),
        "disabled" => budget_to_level(0).map(str::to_owned),
        _ => None,
    }
}

/// The system message for Claude's top-level `system`: a string, or a list
/// of text and image blocks. `None` if nothing in it is kept.
fn system_message(system: Option<&Value>) -> Option<Value> {
    let parts: Vec<Value> = match system? {
        Value::String(text) if text.is_empty() || is_attribution_system_text(text) => Vec::new(),
        Value::String(text) => vec![text_part(text)],
        Value::Array(items) => items.iter().filter_map(convert_content_part).collect(),
        _ => Vec::new(),
    };
    (!parts.is_empty()).then(|| object([("role", "system".into()), ("content", parts.into())]))
}

/// Converts the conversation one Claude message at a time.
struct Messages {
    keep_thinking: bool,
    out: Vec<Value>,
    /// The `tool_use` IDs of the last message with list content, which the
    /// next user message's tool results are ordered by.
    pending_tool_use_ids: Vec<String>,
    /// Reminders from system messages that came while tool calls waited for
    /// their results; they go after the results.
    pending_reminders: Vec<Value>,
    /// Each `tool_use` ID's tool name, from every assistant message so far.
    tool_names: std::collections::HashMap<String, String>,
}

impl Messages {
    fn push(&mut self, message: &Value) {
        let role = str_of(message.get("role"));
        let content = message.get("content");
        if role == "system" {
            if let Some(reminder) = message_system_reminder_text(content) {
                let reminder = object([
                    ("role", "user".into()),
                    ("content", json!([text_part(&reminder)])),
                ]);
                if self.pending_tool_use_ids.is_empty() {
                    self.out.push(reminder);
                } else {
                    self.pending_reminders.push(reminder);
                }
            }
            return;
        }
        match content {
            Some(Value::Array(parts)) => self.push_parts(&role, parts),
            Some(Value::String(text)) => {
                self.out.push(object([
                    ("role", Value::from(&*role)),
                    ("content", text.as_str().into()),
                ]));
            }
            _ => {}
        }
    }

    fn push_parts(&mut self, role: &str, parts: &[Value]) {
        let pending = std::mem::take(&mut self.pending_tool_use_ids);
        let parts = if role == "user" {
            align_tool_results(parts, &pending)
        } else {
            std::borrow::Cow::Borrowed(parts)
        };
        let preceding_tool_calls_pending = !pending.is_empty();

        let mut content = Vec::new();
        let mut reasoning = Vec::new();
        let mut tool_calls = Vec::new();
        let mut tool_results = Vec::new();
        let mut relayed_images = Vec::new();
        for part in parts.iter() {
            match &*str_of(part.get("type")) {
                "thinking" => {
                    if role == "assistant" && self.maps_thinking(part) {
                        let text = thinking_text(part);
                        if !text.trim().is_empty() {
                            reasoning.push(text);
                        }
                    }
                }
                "text" | "image" => content.extend(convert_content_part(part)),
                "tool_use" if role == "assistant" => {
                    let id = str_of(part.get("id"));
                    let name = str_of(part.get("name"));
                    if !id.is_empty() {
                        self.pending_tool_use_ids.push(id.to_string());
                        if !name.is_empty() {
                            self.tool_names.insert(id.to_string(), name.to_string());
                        }
                    }
                    let arguments = match part.get("input") {
                        Some(input) => input.to_string(),
                        None => "{}".to_owned(),
                    };
                    // Upstream builds the call through a Go map, so its keys
                    // come out sorted.
                    tool_calls.push(object([
                        (
                            "function",
                            object([
                                ("arguments", arguments.into()),
                                ("name", Value::from(&*name)),
                            ]),
                        ),
                        ("id", Value::from(&*id)),
                        ("type", "function".into()),
                    ]));
                }
                "tool_result" => {
                    let id = str_of(part.get("tool_use_id"));
                    let (text, images) = convert_tool_result_content(part.get("content"));
                    let mut result = object([
                        ("role", "tool".into()),
                        ("tool_call_id", Value::from(&*id)),
                        ("content", text.into()),
                    ]);
                    if let Some(name) = self.tool_names.get(&*id).filter(|n| !n.is_empty()) {
                        result["name"] = name.as_str().into();
                    }
                    relayed_images.extend(images);
                    tool_results.push(result);
                }
                _ => {}
            }
        }

        let has_content = !content.is_empty();
        let reasoning = reasoning.join("\n\n");

        // Reminders that waited on tool calls no result answers go first.
        if preceding_tool_calls_pending && tool_results.is_empty() {
            self.out.append(&mut self.pending_reminders);
        }
        // Tool messages must follow the assistant message that called them,
        // so they come before this message's own content.
        self.out.append(&mut tool_results);
        if !relayed_images.is_empty() {
            let mut relay = Vec::with_capacity(relayed_images.len() + 1);
            relay.push(text_part(TOOL_RESULT_IMAGE_RELAY_NOTICE));
            relay.append(&mut relayed_images);
            if role == "user" && has_content {
                relay.append(&mut content);
                content = relay;
            } else {
                self.out
                    .push(object([("role", "user".into()), ("content", relay.into())]));
            }
        }
        self.out.append(&mut self.pending_reminders);

        if role == "assistant" {
            if has_content || !reasoning.is_empty() || !tool_calls.is_empty() {
                let mut message = Map::new();
                message.insert("role".into(), "assistant".into());
                let content = if has_content {
                    content.into()
                } else {
                    Value::from("")
                };
                message.insert("content".into(), content);
                if !reasoning.is_empty() {
                    message.insert("reasoning_content".into(), reasoning.into());
                }
                if !tool_calls.is_empty() {
                    message.insert("tool_calls".into(), tool_calls.into());
                }
                self.out.push(Value::Object(message));
            }
        } else if has_content {
            self.out
                .push(object([("role", role.into()), ("content", content.into())]));
        }
    }

    /// Whether an assistant's thinking block is passed on: always in
    /// compatibility mode, else only with a signature a GPT model can replay.
    fn maps_thinking(&self, part: &Value) -> bool {
        if self.keep_thinking {
            return true;
        }
        let Some(signature) = part.get("signature") else {
            return false;
        };
        let signature = str_of(Some(signature));
        !signature.trim().is_empty()
            && compatible_signature_for_provider(Provider::Gpt, &signature).is_some()
    }

    fn finish(mut self) -> Vec<Value> {
        self.out.append(&mut self.pending_reminders);
        self.out
    }
}

fn text_part(text: &str) -> Value {
    object([("type", "text".into()), ("text", text.into())])
}

/// A text or image block as a Chat Completions content part. `None` for
/// blank or attribution text, an image with no URL, and other blocks.
fn convert_content_part(part: &Value) -> Option<Value> {
    match &*str_of(part.get("type")) {
        "text" => {
            let text = str_of(part.get("text"));
            if text.trim().is_empty() || is_attribution_system_text(&text) {
                return None;
            }
            Some(text_part(&text))
        }
        "image" => {
            let mut url = String::new();
            if let Some(source) = part.get("source") {
                match &*str_of(source.get("type")) {
                    "base64" => {
                        let mut media_type = str_of(source.get("media_type"));
                        if media_type.is_empty() {
                            media_type = "application/octet-stream".into();
                        }
                        let data = str_of(source.get("data"));
                        if !data.is_empty() {
                            url = format!("data:{media_type};base64,{data}");
                        }
                    }
                    "url" => url = str_of(source.get("url")).into_owned(),
                    _ => {}
                }
            }
            if url.is_empty() {
                url = str_of(part.get("url")).into_owned();
            }
            if url.is_empty() {
                return None;
            }
            Some(object([
                ("type", "image_url".into()),
                ("image_url", object([("url", url.into())])),
            ]))
        }
        _ => None,
    }
}

/// A tool result's content as the text of a tool message, and the images in
/// it, which go to the next user message instead. Content that is only
/// images gives [`TOOL_RESULT_IMAGE_PLACEHOLDER`]; content that can't be read
/// as text is passed on as JSON.
fn convert_tool_result_content(content: Option<&Value>) -> (String, Vec<Value>) {
    let Some(content) = content else {
        return (String::new(), Vec::new());
    };
    match content {
        Value::String(text) => (text.clone(), Vec::new()),
        Value::Array(items) => {
            let mut parts = Vec::new();
            let mut images = Vec::new();
            for item in items {
                let kind = item.is_object().then(|| str_of(item.get("type")));
                match (item, kind.as_deref()) {
                    (Value::String(text), _) => parts.push(text.clone()),
                    (_, Some("text")) => parts.push(str_of(item.get("text")).into_owned()),
                    (_, Some("image")) => match convert_content_part(item) {
                        Some(image) => images.push(image),
                        None => parts.push(item.to_string()),
                    },
                    (_, Some(_)) if item.get("text").is_some_and(Value::is_string) => {
                        parts.push(str_of(item.get("text")).into_owned());
                    }
                    _ => parts.push(item.to_string()),
                }
            }
            let joined = parts.join("\n\n");
            if joined.trim().is_empty() {
                if images.is_empty() {
                    return (content.to_string(), Vec::new());
                }
                return (TOOL_RESULT_IMAGE_PLACEHOLDER.to_owned(), images);
            }
            (joined, images)
        }
        Value::Object(_) => {
            if str_of(content.get("type")) == "image"
                && let Some(image) = convert_content_part(content)
            {
                return (TOOL_RESULT_IMAGE_PLACEHOLDER.to_owned(), vec![image]);
            }
            match content.get("text") {
                Some(Value::String(text)) => (text.clone(), Vec::new()),
                _ => (content.to_string(), Vec::new()),
            }
        }
        _ => (content.to_string(), Vec::new()),
    }
}

/// A Claude tool as a Chat Completions function tool.
fn convert_tool(tool: &Value) -> Value {
    let parameters = match tool.get("input_schema") {
        Some(schema) if !schema.is_null() => normalize_schema(schema.clone()),
        _ => json!({"type": "object", "properties": {}}),
    };
    object([
        ("type", "function".into()),
        (
            "function",
            object([
                ("name", str_of(tool.get("name")).into()),
                ("description", str_of(tool.get("description")).into()),
                ("parameters", parameters),
            ]),
        ),
    ])
}

/// `normalizeObjectSchemaProperties`: makes a schema fit for strict
/// validators. A `true` subschema becomes `{}` (`false` stays), an object
/// schema without `properties` gets empty ones, and a `pattern` with a
/// Unicode property escape is removed, as is a `patternProperties` entry
/// keyed by one. `additionalProperties: true` is kept, and values that are
/// data rather than schemas, such as `default` and `enum`, are left alone.
fn normalize_schema(schema: Value) -> Value {
    match schema {
        Value::Bool(true) => json!({}),
        Value::Object(mut fields) => {
            if fields.get("type").and_then(Value::as_str) == Some("object")
                && !fields.contains_key("properties")
            {
                fields.insert("properties".into(), json!({}));
            }
            if fields
                .get("pattern")
                .and_then(Value::as_str)
                .is_some_and(has_unsupported_unicode_property_escape)
            {
                fields.shift_remove("pattern");
            }
            if let Some(Value::Object(patterns)) = fields.get_mut("patternProperties") {
                patterns.retain(|key, _| !has_unsupported_unicode_property_escape(key));
                for subschema in patterns.values_mut() {
                    *subschema = normalize_schema(subschema.take());
                }
            }
            for keyword in MAP_KEYWORDS {
                if keyword == "patternProperties" {
                    continue;
                }
                if let Some(Value::Object(subschemas)) = fields.get_mut(keyword) {
                    for subschema in subschemas.values_mut() {
                        *subschema = normalize_schema(subschema.take());
                    }
                }
            }
            for keyword in VALUE_KEYWORDS {
                match fields.get_mut(keyword) {
                    // Boolean `additionalProperties` is kept for structured
                    // outputs.
                    Some(value @ Value::Bool(true)) if keyword != "additionalProperties" => {
                        *value = json!({});
                    }
                    Some(value @ Value::Object(_)) => *value = normalize_schema(value.take()),
                    Some(Value::Array(items)) => {
                        for item in items.iter_mut() {
                            *item = normalize_schema(item.take());
                        }
                    }
                    _ => {}
                }
            }
            Value::Object(fields)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(normalize_schema).collect()),
        other => other,
    }
}

/// Claude's `tool_choice` in Chat Completions terms. An unknown choice, or a
/// named tool without a name, becomes `"none"` rather than granting more.
fn convert_tool_choice(choice: &Value) -> Value {
    let mut kind = str_of(choice.get("type"));
    if kind.is_empty()
        && let Value::String(text) = choice
    {
        kind = text.as_str().into();
    }
    match &*kind {
        "auto" => "auto".into(),
        "any" => "required".into(),
        "tool" => {
            let name = str_of(choice.get("name"));
            if name.is_empty() {
                "none".into()
            } else {
                json!({"type": "function", "function": {"name": name}})
            }
        }
        _ => "none".into(),
    }
}

#[cfg(test)]
mod tests;
