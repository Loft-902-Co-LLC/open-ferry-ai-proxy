// Ported from CLIProxyAPI internal/translator/claude/openai/chat-completions/claude_openai_request.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! OpenAI Chat Completions request → Claude Messages request.
//!
//! System and developer messages become Claude's top-level `system` blocks.
//! The rest become alternating user and assistant turns: an assistant's tool
//! calls become `tool_use` blocks, and each tool message a `tool_result` in a
//! user turn. `reasoning_effort` becomes adaptive thinking with an effort for
//! models that take one, or a thinking budget for older models; which kind a
//! model takes comes from the [`ModelCatalog`].
//!
//! Deviations from upstream:
//! - `metadata.user_id` is only set to an ID the client sent, in
//!   `metadata.user_id` or `user`. Without one, upstream derives an ID from
//!   the conversation; we don't make up user IDs.
//! - A tool call without an ID gets one in upstream's form, `toolu_` and 24
//!   letters and digits, drawn from the standard library's randomly keyed
//!   hasher rather than the operating system's random source.
//! - A `top_p` that isn't a finite number, such as `1e400` or the string
//!   `"NaN"`, is left out. Go writes it as `+Inf` or `NaN`, which isn't JSON.
//! - Where upstream copies the client's JSON text into a string, we write the
//!   same JSON compactly. This applies to the schema in a structured output
//!   instruction, a tool message's content that can't be converted, and a
//!   non-string value read as text.
//! - Tool call arguments nested more than 128 levels deep read as `{}`, past
//!   serde_json's limit.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use crate::common::cache_control;
use crate::common::claude::{
    MessageAccumulator, apply_reasoning_effort, client_user_id, generate_tool_call_id,
    sanitize_function_name, sanitize_tool_id, structured_output_instruction,
};
use crate::go;
use crate::json::{float_of, int_of, object, path, str_of};
use crate::models::ModelCatalog;
use crate::schema::normalize_claude_tool_input_schema;
use crate::thinking::summary::apply_translated_to_claude;

const DEFAULT_MAX_TOKENS: i64 = 32000;

/// Converts a Chat Completions request body into a Claude Messages request
/// body for `model_name`. `stream` is whether the client asked to stream.
/// `models` says which thinking settings the model takes; pass
/// [`ModelCatalog::embedded`] unless you have your own.
pub fn convert_openai_chat_completions_request_to_claude(
    model_name: &str,
    request: &Value,
    stream: bool,
    models: &ModelCatalog,
) -> Value {
    convert(model_name, request, stream, models, false)
}

/// [`convert_openai_chat_completions_request_to_claude`] for compatibility
/// endpoints, which also get back an assistant's `reasoning_content` as an
/// unsigned thinking block.
pub fn convert_openai_chat_completions_request_to_claude_with_compat(
    model_name: &str,
    request: &Value,
    stream: bool,
    models: &ModelCatalog,
) -> Value {
    convert(model_name, request, stream, models, true)
}

fn convert(
    model_name: &str,
    request: &Value,
    stream: bool,
    models: &ModelCatalog,
    keep_reasoning: bool,
) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), model_name.into());
    out.insert("max_tokens".into(), DEFAULT_MAX_TOKENS.into());
    out.insert("messages".into(), json!([]));
    let metadata = match client_user_id(request) {
        Some(user_id) => object([("user_id", user_id.into())]),
        None => json!({}),
    };
    out.insert("metadata".into(), metadata);

    if let Some(effort) = request.get("reasoning_effort") {
        apply_reasoning_effort(&mut out, &str_of(Some(effort)), model_name, models);
    }
    // OpenAI deprecated max_tokens for max_completion_tokens, so either works.
    if let Some(max_tokens) = request
        .get("max_tokens")
        .or_else(|| request.get("max_completion_tokens"))
    {
        out.insert("max_tokens".into(), int_of(max_tokens).into());
    }
    if let Some(top_p) = request.get("top_p").and_then(float_of) {
        out.insert("top_p".into(), top_p);
    }
    if let Some(stop) = request.get("stop") {
        let sequences: Vec<Value> = match stop {
            Value::Array(items) => items.iter().map(|item| str_of(Some(item)).into()).collect(),
            other => vec![str_of(Some(other)).into()],
        };
        if !sequences.is_empty() {
            out.insert("stop_sequences".into(), sequences.into());
        }
    }
    out.insert("stream".into(), stream.into());

    let mut system = Vec::new();
    let mut messages = match request.get("messages") {
        Some(Value::Array(messages)) => convert_messages(messages, &mut system, keep_reasoning),
        _ => Vec::new(),
    };
    if let Some(instruction) = structured_output_instruction(request.get("response_format")) {
        system.push(text_block(instruction));
    }
    // A request of only system messages still needs a turn.
    if messages.is_empty() && !system.is_empty() {
        messages.push(json!({"role": "user", "content": [{"type": "text", "text": ""}]}));
    }
    if !system.is_empty() {
        out.insert("system".into(), system.into());
    }
    if !messages.is_empty() {
        out.insert("messages".into(), messages.into());
    }

    let allowed = allowed_tools(request.get("tool_choice"));
    let mut tool_count = 0;
    if let Some(Value::Array(tools)) = request.get("tools")
        && !tools.is_empty()
    {
        let tools: Vec<Value> = tools
            .iter()
            .filter_map(|tool| convert_tool(tool, allowed.as_ref()))
            .collect();
        tool_count = tools.len();
        if !tools.is_empty() {
            out.insert("tools".into(), tools.into());
        }
    }
    let tool_choice = match &allowed {
        Some(_) if tool_count == 0 => Some(json!({"type": "none"})),
        Some(allowed) if allowed.mode == "required" => Some(json!({"type": "any"})),
        Some(_) => Some(json!({"type": "auto"})),
        None => convert_tool_choice(request.get("tool_choice")),
    };
    if let Some(tool_choice) = tool_choice {
        out.insert("tool_choice".into(), tool_choice);
    }
    if request.get("parallel_tool_calls") == Some(&Value::Bool(false)) {
        if let Some(Value::Object(choice)) = out.get_mut("tool_choice") {
            if str_of(choice.get("type")) != "none" {
                choice.insert("disable_parallel_tool_use".into(), true.into());
            }
        } else if out.contains_key("tools") {
            out.insert(
                "tool_choice".into(),
                json!({"type": "auto", "disable_parallel_tool_use": true}),
            );
        }
    }

    let mut out = Value::Object(out);
    apply_translated_to_claude(&mut out, request, "openai", model_name, models);
    out
}

/// Converts the messages into Claude turns, collecting system blocks into
/// `system` on the way.
fn convert_messages(
    messages: &[Value],
    system: &mut Vec<Value>,
    keep_reasoning: bool,
) -> Vec<Value> {
    // A call answered more than once takes its last answer, in the place of
    // its first.
    let mut last_answers = HashMap::new();
    for message in messages {
        let id = str_of(message.get("tool_call_id"));
        if str_of(message.get("role")) == "tool" && !id.is_empty() {
            last_answers.insert(id, message);
        }
    }
    let mut answered = HashSet::new();

    let mut turns = MessageAccumulator::default();
    for message in messages {
        match &*str_of(message.get("role")) {
            // Developer messages rank with system messages in OpenAI's
            // instruction hierarchy, so both become system blocks.
            "system" | "developer" => system_blocks(message, system),
            "user" => turns.push("user", message_blocks(message, false)),
            "assistant" => turns.push("assistant", message_blocks(message, keep_reasoning)),
            "tool" => {
                let raw_id = str_of(message.get("tool_call_id"));
                let id = sanitize_tool_id(&raw_id);
                let answer = if raw_id.is_empty() {
                    message
                } else if answered.insert(raw_id.clone()) {
                    last_answers.get(&raw_id).copied().unwrap_or(message)
                } else {
                    continue;
                };
                let result = object([
                    ("type", "tool_result".into()),
                    ("tool_use_id", id.into()),
                    ("content", tool_result_content(answer.get("content"))),
                ]);
                let mut blocks = vec![result];
                cache_control::attach_to_tool_result(&mut blocks, answer);
                turns.push("user", blocks);
            }
            _ => {}
        }
    }
    turns.into_messages()
}

fn system_blocks(message: &Value, system: &mut Vec<Value>) {
    match message.get("content") {
        Some(Value::String(text)) if !text.is_empty() => {
            let mut block = text_block(text.as_str());
            cache_control::attach_to(&mut block, message);
            system.push(block);
        }
        Some(Value::Array(parts)) => {
            let start = system.len();
            for part in parts {
                if str_of(part.get("type")) == "text" {
                    let mut block = text_block(str_of(part.get("text")));
                    cache_control::attach_to(&mut block, part);
                    system.push(block);
                }
            }
            // A message's marker goes on its last block.
            if message.get("cache_control").is_some() && system.len() > start {
                cache_control::attach_to_last_block(&mut system[start..], message);
            }
        }
        _ => {}
    }
}

/// The blocks of a user or assistant message: its text and content parts,
/// then, for an assistant, its tool calls.
fn message_blocks(message: &Value, keep_reasoning: bool) -> Vec<Value> {
    let mut blocks = Vec::new();
    if keep_reasoning
        && let Some(Value::String(reasoning)) = message.get("reasoning_content")
        && !reasoning.trim().is_empty()
    {
        blocks.push(object([
            ("type", "thinking".into()),
            ("thinking", reasoning.as_str().into()),
            ("signature", "".into()),
        ]));
    }
    match message.get("content") {
        Some(Value::String(text)) if !text.is_empty() => blocks.push(text_block(text.as_str())),
        Some(Value::Array(parts)) => {
            blocks.extend(parts.iter().filter_map(|part| {
                let mut block = content_part(part)?;
                cache_control::attach_to(&mut block, part);
                Some(block)
            }));
        }
        _ => {}
    }
    if str_of(message.get("role")) == "assistant"
        && let Some(Value::Array(calls)) = message.get("tool_calls")
    {
        blocks.extend(
            calls
                .iter()
                .filter(|call| str_of(call.get("type")) == "function")
                .map(tool_use),
        );
    }
    cache_control::attach_to_last_block(&mut blocks, message);
    blocks
}

fn tool_use(call: &Value) -> Value {
    let mut id = str_of(call.get("id")).into_owned();
    if id.is_empty() {
        id = generate_tool_call_id();
    }
    let function = call.get("function");
    let name = str_of(function.and_then(|function| function.get("name")));
    // Claude wants an object; arguments that aren't one become `{}`.
    let input = function
        .and_then(|function| function.get("arguments"))
        .map(|arguments| str_of(Some(arguments)))
        .and_then(|arguments| serde_json::from_str::<Value>(&arguments).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    object([
        ("type", "tool_use".into()),
        ("id", sanitize_tool_id(&id).into()),
        ("name", sanitize_function_name(&name).into()),
        ("input", input),
    ])
}

/// A tool message's content as a `tool_result`'s: text stays text, and an
/// array or a single part becomes Claude blocks. Anything else is written
/// as JSON text.
fn tool_result_content(content: Option<&Value>) -> Value {
    match content {
        None => "".into(),
        Some(Value::String(text)) => text.as_str().into(),
        Some(Value::Array(parts)) => {
            let blocks: Vec<Value> = parts
                .iter()
                .filter_map(|part| match part {
                    Value::String(text) => Some(text_block(text.as_str())),
                    part => content_part(part),
                })
                .collect();
            if blocks.is_empty() && !parts.is_empty() {
                content.map(Value::to_string).unwrap_or_default().into()
            } else {
                blocks.into()
            }
        }
        Some(part @ Value::Object(_)) => match content_part(part) {
            Some(block) => vec![block].into(),
            None => part.to_string().into(),
        },
        Some(other) => other.to_string().into(),
    }
}

/// A Chat Completions content part as a Claude block: text, an image, or a
/// file given as a data URL. Other parts have none.
fn content_part(part: &Value) -> Option<Value> {
    match &*str_of(part.get("type")) {
        "text" => Some(text_block(str_of(part.get("text")))),
        "image_url" => image_block(&str_of(path(part, "image_url.url"))),
        "file" => {
            let data = str_of(path(part, "file.file_data"));
            let rest = data.strip_prefix("data:")?;
            let semicolon = data.find(';')?;
            let comma = data.find(',')?;
            if comma < semicolon {
                return None;
            }
            Some(json!({
                "type": "document",
                "source": {
                    "type": "base64",
                    "media_type": &rest[..semicolon - "data:".len()],
                    "data": &data[comma + 1..],
                },
            }))
        }
        _ => None,
    }
}

/// An image from a URL, or from the base64 data in a data URL.
fn image_block(url: &str) -> Option<Value> {
    if url.is_empty() {
        return None;
    }
    let Some(rest) = url.strip_prefix("data:") else {
        return Some(json!({"type": "image", "source": {"type": "url", "url": url}}));
    };
    let (header, data) = rest.split_once(',')?;
    let media_type = header.split(';').next().unwrap_or_default();
    let media_type = if media_type.is_empty() {
        "application/octet-stream"
    } else {
        media_type
    };
    Some(json!({
        "type": "image",
        "source": {"type": "base64", "media_type": media_type, "data": data},
    }))
}

fn text_block(text: impl Into<Value>) -> Value {
    object([("type", "text".into()), ("text", text.into())])
}

/// What `allowed_tools` in `tool_choice` permits.
struct AllowedTools {
    /// Each name as given and as sanitized for Claude.
    names: HashSet<String>,
    /// `auto` or `required`, as the client wrote it.
    mode: String,
}

fn allowed_tools(tool_choice: Option<&Value>) -> Option<AllowedTools> {
    let choice = tool_choice.filter(|choice| choice.is_object())?;
    if str_of(choice.get("type")) != "allowed_tools" {
        return None;
    }
    let mut tools = gjson_array(path(choice, "allowed_tools.tools"));
    if tools.is_empty() {
        tools = gjson_array(choice.get("tools"));
    }
    let mut names = HashSet::new();
    for tool in tools {
        let mut name = str_of(path(tool, "function.name")).trim().to_owned();
        if name.is_empty() {
            name = str_of(tool.get("name")).trim().to_owned();
        }
        if !name.is_empty() {
            names.insert(sanitize_function_name(&name));
            names.insert(name);
        }
    }
    let mode = |key: &str| go::to_lower(str_of(path(choice, key)).trim());
    let mut mode_value = mode("allowed_tools.mode");
    if mode_value.is_empty() {
        mode_value = mode("mode");
    }
    if mode_value.is_empty() {
        mode_value = "auto".into();
    }
    Some(AllowedTools {
        names,
        mode: mode_value,
    })
}

/// gjson `Array()`: an array's items, nothing for `null` or a missing value,
/// and any other value alone.
fn gjson_array(value: Option<&Value>) -> Vec<&Value> {
    match value {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.iter().collect(),
        Some(other) => vec![other],
    }
}

fn convert_tool(tool: &Value, allowed: Option<&AllowedTools>) -> Option<Value> {
    if str_of(tool.get("type")) != "function" {
        return None;
    }
    let function = tool.get("function").unwrap_or(&Value::Null);
    let name = str_of(function.get("name"));
    let sanitized = sanitize_function_name(&name);
    if let Some(allowed) = allowed
        && !allowed.names.contains(&*name)
        && !allowed.names.contains(&sanitized)
    {
        return None;
    }
    let schema = function
        .get("parameters")
        .or_else(|| function.get("parametersJsonSchema"));
    let mut out = Map::new();
    out.insert("name".into(), sanitized.into());
    out.insert(
        "description".into(),
        str_of(function.get("description")).into(),
    );
    out.insert(
        "input_schema".into(),
        normalize_claude_tool_input_schema(schema),
    );
    cache_control::attach(&mut out, tool);
    if !out.contains_key("cache_control") {
        cache_control::attach(&mut out, function);
    }
    if let Some(Value::Bool(strict)) = function.get("strict").or_else(|| tool.get("strict")) {
        out.insert("strict".into(), (*strict).into());
    }
    Some(Value::Object(out))
}

fn convert_tool_choice(tool_choice: Option<&Value>) -> Option<Value> {
    let kind = match tool_choice? {
        Value::String(choice) => choice.as_str(),
        choice @ (Value::Object(_) | Value::Array(_)) => {
            let kind = str_of(choice.get("type"));
            if kind == "function" {
                let mut name = str_of(path(choice, "function.name"));
                if name.is_empty() {
                    name = str_of(choice.get("name"));
                }
                if name.is_empty() {
                    return Some(json!({"type": "none"}));
                }
                return Some(json!({"type": "tool", "name": sanitize_function_name(&name)}));
            }
            return match &*kind {
                "none" => Some(json!({"type": "none"})),
                "auto" => Some(json!({"type": "auto"})),
                "required" | "any" => Some(json!({"type": "any"})),
                _ => None,
            };
        }
        _ => return None,
    };
    match kind {
        "none" => Some(json!({"type": "none"})),
        "auto" => Some(json!({"type": "auto"})),
        "required" => Some(json!({"type": "any"})),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
