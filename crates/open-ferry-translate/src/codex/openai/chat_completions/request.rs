// Ported from CLIProxyAPI internal/translator/codex/openai/chat-completions/codex_openai_request.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! OpenAI Chat Completions request → Codex request.
//!
//! Messages become Responses input items. An assistant message's tool calls
//! become `function_call` or `custom_tool_call` items, and each tool message
//! the output item of the call it answers. Tool names are made safe for Codex
//! and fit within its 64-byte limit; the response translator restores them.
//!
//! Deviations from upstream:
//! - Where upstream copies the client's JSON text into a string, we write the
//!   same JSON compactly. This applies to a tool message's content that is
//!   neither a string nor an array, a tool output part it doesn't recognize,
//!   and a non-string value read as text.
//! - A tool message's string content is read as JSON only if all of it is
//!   valid JSON. gjson reads what it can from malformed JSON.
//! - A number too large for `f64`, where upstream copies a value as a number
//!   (such as `reasoning_effort`), is kept as written. Go can't write it, and
//!   upstream's output breaks.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use super::super::super::unique_names::UniqueNames;
use crate::apply_patch;
use crate::go;
use crate::json::{go_value, object, path, str_of};

/// Codex's limit on tool name length, in bytes.
const NAME_LIMIT: usize = 64;

/// A tool name the client used → the name Codex gets.
pub(super) type ToolNameMap = HashMap<String, String>;

/// Converts a Chat Completions request body into a Codex Responses request
/// body for `model_name`. `stream` is whether the client asked to stream.
pub fn convert_openai_chat_completions_request_to_codex(
    model_name: &str,
    request: &Value,
    stream: bool,
) -> Value {
    let tools = match request.get("tools") {
        Some(Value::Array(tools)) if !tools.is_empty() => Some(tools),
        _ => None,
    };
    let names = ToolNames {
        short: build_short_name_map(&collect_request_tool_names(request)),
        custom: custom_tool_names(tools),
    };

    let mut out = Map::new();
    out.insert("instructions".into(), "".into());
    out.insert("stream".into(), stream.into());
    let effort = request
        .get("reasoning_effort")
        .map_or_else(|| "medium".into(), go_value);
    out.insert("reasoning".into(), object([("effort", effort)]));
    if let Some(tier) = service_tier(request.get("service_tier")) {
        out.insert("service_tier".into(), tier.into());
    }
    out.insert("parallel_tool_calls".into(), true.into());
    // Reasoning summaries are opt-in on the Responses API, so only the effort is set.
    out.insert("include".into(), json!(["reasoning.encrypted_content"]));
    out.insert("model".into(), model_name.into());
    out.insert("input".into(), input_items(request, &names).into());
    if let Some(text) = text_settings(request) {
        out.insert("text".into(), text);
    }
    if let Some(tools) = tools {
        let tools: Vec<Value> = tools
            .iter()
            .filter_map(|tool| convert_tool(tool, &names.short))
            .collect();
        out.insert("tools".into(), tools.into());
    }
    if let Some(choice) = convert_tool_choice(request.get("tool_choice"), &names) {
        out.insert("tool_choice".into(), choice);
    }
    out.insert("store".into(), false.into());
    Value::Object(out)
}

struct ToolNames {
    short: ToolNameMap,
    /// Names declared only as custom tools. A name also declared as a function
    /// stays a function, since a function call can't say which one it meant.
    custom: HashSet<String>,
}

impl ToolNames {
    fn codex_name(&self, name: &str) -> String {
        codex_tool_name(&self.short, name)
    }
}

fn custom_tool_names(tools: Option<&Vec<Value>>) -> HashSet<String> {
    let mut functions = HashSet::new();
    let mut customs = HashSet::new();
    for tool in tools.into_iter().flatten() {
        match &*str_of(tool.get("type")) {
            "function" => {
                functions.insert(str_of(path(tool, "function.name")).into_owned());
            }
            "custom" => {
                customs.insert(str_of(tool.get("name")).into_owned());
            }
            _ => {}
        }
    }
    customs.retain(|name| !functions.contains(name));
    customs
}

/// An assistant tool call, as upstream's `resolveToolCall` reads it.
struct ToolCall<'v> {
    custom: bool,
    name: Cow<'v, str>,
    input: String,
}

fn resolve_tool_call<'v>(call: &'v Value, custom_names: &HashSet<String>) -> Option<ToolCall<'v>> {
    match &*str_of(call.get("type")) {
        "custom" => Some(ToolCall {
            custom: true,
            name: str_of(path(call, "custom.name")),
            input: str_of(path(call, "custom.input")).into_owned(),
        }),
        "function" => {
            let name = str_of(path(call, "function.name"));
            let custom = custom_names.contains(&*name);
            let mut input = str_of(path(call, "function.arguments")).into_owned();
            // A custom apply_patch call in function form carries its patch in
            // a JSON envelope. Explicit custom calls carry the raw patch.
            if custom
                && name.trim() == "apply_patch"
                && let Some(patch) = apply_patch::unwrap_input(&input)
            {
                input = patch;
            }
            Some(ToolCall {
                custom,
                name,
                input,
            })
        }
        _ => None,
    }
}

/// A tool call waiting for its tool message.
struct PendingCall {
    call_id: String,
    source_call_id: String,
    custom: bool,
    consumed: bool,
}

fn input_items(request: &Value, names: &ToolNames) -> Vec<Value> {
    let Some(Value::Array(messages)) = request.get("messages") else {
        return Vec::new();
    };
    let mut items = Vec::with_capacity(messages.len());
    let mut pending: Vec<PendingCall> = Vec::new();
    // IDs used by more than one call in the latest batch. Their outputs can't
    // be matched up, so they're dropped along with the calls.
    let mut ambiguous: HashSet<String> = HashSet::new();
    for (i, message) in messages.iter().enumerate() {
        let role = str_of(message.get("role"));
        if role == "tool" {
            let id = str_of(message.get("tool_call_id"));
            if !id.is_empty() && ambiguous.contains(&*id) {
                continue;
            }
            let call = pending.iter_mut().find(|call| {
                !call.consumed && (id.is_empty() || call.source_call_id == id || call.call_id == id)
            });
            let Some(call) = call else {
                continue;
            };
            call.consumed = true;
            let kind = if call.custom {
                "custom_tool_call_output"
            } else {
                "function_call_output"
            };
            items.push(object([
                ("type", kind.into()),
                ("call_id", call.call_id.clone().into()),
                ("output", tool_output(message.get("content"))),
            ]));
            continue;
        }

        // Any other message starts a new batch of tool calls.
        pending.clear();
        ambiguous.clear();
        let content = message_content(&role, message.get("content"));
        // An assistant message holding only tool calls becomes just the call
        // items, or Codex can't match call IDs.
        if role != "assistant" || !content.is_empty() {
            let codex_role = if role == "system" { "developer" } else { &role };
            items.push(object([
                ("type", "message".into()),
                ("role", codex_role.into()),
                ("content", content.into()),
            ]));
        }
        if role == "assistant"
            && let Some(Value::Array(calls)) = message.get("tool_calls")
        {
            push_tool_calls(calls, i, names, &mut pending, &mut ambiguous, &mut items);
        }
    }
    items
}

/// Adds an assistant message's tool calls to `items`, and to `pending` for
/// the tool messages that follow. Calls sharing an ID are dropped.
fn push_tool_calls(
    calls: &[Value],
    message_index: usize,
    names: &ToolNames,
    pending: &mut Vec<PendingCall>,
    ambiguous: &mut HashSet<String>,
    items: &mut Vec<Value>,
) {
    let resolved: Vec<Option<ToolCall<'_>>> = calls
        .iter()
        .map(|call| resolve_tool_call(call, &names.custom))
        .collect();
    let mut counts: HashMap<Cow<'_, str>, usize> = HashMap::new();
    let mut used: HashSet<String> = HashSet::new();
    for (call, resolved) in calls.iter().zip(&resolved) {
        let id = str_of(call.get("id"));
        if resolved.is_some() && !id.is_empty() {
            used.insert(id.clone().into_owned());
            *counts.entry(id).or_default() += 1;
        }
    }
    ambiguous.extend(
        counts
            .into_iter()
            .filter(|&(_, count)| count > 1)
            .map(|(id, _)| id.into_owned()),
    );

    for (j, (call, resolved)) in calls.iter().zip(resolved).enumerate() {
        let Some(resolved) = resolved else {
            continue;
        };
        let source_call_id = str_of(call.get("id"));
        if !source_call_id.is_empty() && ambiguous.contains(&*source_call_id) {
            continue;
        }
        let call_id = if source_call_id.is_empty() {
            let base = format!("call_missing_{message_index}_{j}");
            let mut call_id = base.clone();
            let mut suffix = 1;
            while used.contains(&call_id) {
                call_id = format!("{base}_{suffix}");
                suffix += 1;
            }
            used.insert(call_id.clone());
            call_id
        } else {
            source_call_id.clone().into_owned()
        };
        pending.push(PendingCall {
            call_id: call_id.clone(),
            source_call_id: source_call_id.into_owned(),
            custom: resolved.custom,
            consumed: false,
        });

        let name = names.codex_name(&resolved.name);
        items.push(if resolved.custom {
            object([
                ("type", "custom_tool_call".into()),
                ("call_id", call_id.into()),
                ("name", name.into()),
                ("input", resolved.input.into()),
            ])
        } else {
            object([
                ("type", "function_call".into()),
                ("call_id", call_id.into()),
                ("name", name.into()),
                ("arguments", resolved.input.into()),
            ])
        });
    }
}

fn message_content(role: &str, content: Option<&Value>) -> Vec<Value> {
    let text_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    match content {
        Some(Value::String(text)) if !text.is_empty() => {
            vec![object([
                ("type", text_type.into()),
                ("text", text.as_str().into()),
            ])]
        }
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| content_part(role, text_type, part))
            .collect(),
        _ => Vec::new(),
    }
}

fn content_part(role: &str, text_type: &str, part: &Value) -> Option<Value> {
    match &*str_of(part.get("type")) {
        "text" => Some(object([
            ("type", text_type.into()),
            ("text", str_of(part.get("text")).into()),
        ])),
        "image_url" if role == "user" => {
            let mut image = Map::new();
            image.insert("type".into(), "input_image".into());
            if let Some(url) = path(part, "image_url.url") {
                image.insert("image_url".into(), str_of(Some(url)).into());
            }
            Some(Value::Object(image))
        }
        "file" if role == "user" => {
            let data = str_of(path(part, "file.file_data"));
            if data.is_empty() {
                return None;
            }
            let mut file = Map::new();
            file.insert("type".into(), "input_file".into());
            file.insert("file_data".into(), data.into());
            let filename = str_of(path(part, "file.filename"));
            if !filename.is_empty() {
                file.insert("filename".into(), filename.into());
            }
            Some(Value::Object(file))
        }
        "input_audio" if role == "user" => {
            let data = str_of(path(part, "input_audio.data"));
            if data.is_empty() {
                return None;
            }
            let mut audio = Map::new();
            audio.insert("type".into(), "input_audio".into());
            audio.insert("data".into(), data.into());
            let format = str_of(path(part, "input_audio.format"));
            if !format.is_empty() {
                audio.insert("format".into(), format.into());
            }
            Some(Value::Object(audio))
        }
        _ => None,
    }
}

/// A tool message's content as a tool call's `output`: text, or a list of
/// content parts. A string holding a JSON list of parts with an image becomes
/// that list, so the image reaches the model.
fn tool_output(content: Option<&Value>) -> Value {
    match content {
        Some(Value::String(text)) => {
            if text.trim_start().starts_with('[')
                && let Ok(parts) = serde_json::from_str::<Value>(text)
                && has_image_part(&parts)
            {
                return tool_output(Some(&parts));
            }
            text.as_str().into()
        }
        Some(Value::Array(parts)) => parts.iter().map(tool_output_part).collect(),
        Some(other) => other.to_string().into(),
        None => "".into(),
    }
}

fn tool_output_part(part: &Value) -> Value {
    let kind = str_of(part.get("type"));
    match &*kind {
        "text" | "input_text" | "output_text" => object([
            ("type", "input_text".into()),
            ("text", str_of(part.get("text")).into()),
        ]),
        "image_url" | "input_image" => {
            let (url, file_id, detail) = if kind == "input_image" {
                (
                    str_of(part.get("image_url")),
                    str_of(part.get("file_id")),
                    str_of(part.get("detail")),
                )
            } else {
                (
                    str_of(path(part, "image_url.url")),
                    str_of(path(part, "image_url.file_id")),
                    str_of(path(part, "image_url.detail")),
                )
            };
            if url.is_empty() && file_id.is_empty() {
                return fallback_part(part);
            }
            let mut image = Map::new();
            image.insert("type".into(), "input_image".into());
            for (key, value) in [("image_url", url), ("file_id", file_id), ("detail", detail)] {
                if !value.is_empty() {
                    image.insert(key.into(), value.into());
                }
            }
            Value::Object(image)
        }
        "file" => {
            let fields = [
                ("file_id", str_of(path(part, "file.file_id"))),
                ("file_data", str_of(path(part, "file.file_data"))),
                ("file_url", str_of(path(part, "file.file_url"))),
            ];
            if fields.iter().all(|(_, value)| value.is_empty()) {
                return fallback_part(part);
            }
            let mut file = Map::new();
            file.insert("type".into(), "input_file".into());
            let filename = ("filename", str_of(path(part, "file.filename")));
            for (key, value) in fields.into_iter().chain([filename]) {
                if !value.is_empty() {
                    file.insert(key.into(), value.into());
                }
            }
            Value::Object(file)
        }
        _ => fallback_part(part),
    }
}

/// A tool output part we don't recognize, passed on as its JSON text.
fn fallback_part(part: &Value) -> Value {
    object([
        ("type", "input_text".into()),
        ("text", part.to_string().into()),
    ])
}

fn has_image_part(content: &Value) -> bool {
    let Value::Array(parts) = content else {
        return false;
    };
    parts.iter().any(|part| match &*str_of(part.get("type")) {
        "image_url" => {
            !str_of(path(part, "image_url.url")).is_empty()
                || !str_of(path(part, "image_url.file_id")).is_empty()
        }
        "input_image" => {
            !str_of(part.get("image_url")).is_empty() || !str_of(part.get("file_id")).is_empty()
        }
        _ => false,
    })
}

/// `response_format` and `text.verbosity` → Responses `text`.
fn text_settings(request: &Value) -> Option<Value> {
    let format = request.get("response_format");
    let verbosity = request.get("text").and_then(|text| text.get("verbosity"));
    if format.is_none() && verbosity.is_none() {
        return None;
    }
    let mut text = Map::new();
    match format.map(|format| (str_of(format.get("type")), format)) {
        Some((kind, _)) if kind == "text" => {
            text.insert("format".into(), object([("type", "text".into())]));
        }
        Some((kind, format)) if kind == "json_schema" => {
            if let Some(schema) = format.get("json_schema") {
                let mut format = Map::new();
                format.insert("type".into(), "json_schema".into());
                if let Some(name) = schema.get("name") {
                    format.insert("name".into(), go_value(name));
                }
                if let Some(strict) = schema.get("strict") {
                    format.insert("strict".into(), go_value(strict));
                }
                if let Some(schema) = schema.get("schema") {
                    format.insert("schema".into(), schema.clone());
                }
                text.insert("format".into(), Value::Object(format));
            }
        }
        _ => {}
    }
    if let Some(verbosity) = verbosity {
        text.insert("verbosity".into(), go_value(verbosity));
    }
    Some(Value::Object(text))
}

/// Chat Completions tool → Responses tool. Function tools are flattened,
/// custom and built-in tools pass through, and anything else is dropped.
fn convert_tool(tool: &Value, short_names: &ToolNameMap) -> Option<Value> {
    let kind = str_of(tool.get("type"));
    if kind == "custom" {
        let name = codex_tool_name(short_names, &str_of(tool.get("name")));
        let mut tool = tool.clone();
        tool.as_object_mut()?.insert("name".into(), name.into());
        return Some(tool);
    }
    if kind != "function" {
        return (!kind.is_empty() && tool.is_object()).then(|| tool.clone());
    }
    let mut out = Map::new();
    out.insert("type".into(), "function".into());
    if let Some(function) = tool.get("function") {
        if let Some(name) = function.get("name") {
            let name = codex_tool_name(short_names, &str_of(Some(name)));
            out.insert("name".into(), name.into());
        }
        if let Some(description) = function.get("description") {
            out.insert("description".into(), go_value(description));
        }
        if let Some(parameters) = function.get("parameters") {
            out.insert("parameters".into(), parameters.clone());
        }
        // Chat Completions defaults `strict` to false and Responses to true.
        let strict = function.get("strict").map_or(false.into(), go_value);
        out.insert("strict".into(), strict);
    }
    Some(Value::Object(out))
}

/// Chat Completions `tool_choice` → Responses `tool_choice`. Named choices are
/// flattened to `{"type", "name"}`; built-in ones pass through.
fn convert_tool_choice(choice: Option<&Value>, names: &ToolNames) -> Option<Value> {
    let choice = match choice? {
        Value::String(choice) => return Some(choice.as_str().into()),
        choice @ Value::Object(_) => choice,
        _ => return None,
    };
    let mut kind = str_of(choice.get("type"));
    if kind != "function" && kind != "custom" {
        return (!kind.is_empty()).then(|| choice.clone());
    }
    let name = if kind == "function" {
        let name = str_of(path(choice, "function.name"));
        if names.custom.contains(&*name) {
            kind = Cow::Borrowed("custom");
        }
        name
    } else {
        str_of(choice.get("name"))
    };
    let mut out = Map::new();
    out.insert("type".into(), kind.into());
    if !name.is_empty() {
        out.insert("name".into(), names.codex_name(&name).into());
    }
    Some(Value::Object(out))
}

fn service_tier(tier: Option<&Value>) -> Option<&'static str> {
    let tier = tier?.as_str()?;
    match go::to_lower(tier.trim()).as_str() {
        "fast" | "priority" => Some("priority"),
        "ultrafast" => Some("ultrafast"),
        _ => None,
    }
}

fn codex_tool_name(short_names: &ToolNameMap, name: &str) -> String {
    match short_names.get(name) {
        Some(short) => short.clone(),
        None => shorten_name_if_needed(name),
    }
}

/// Replaces each character outside `[A-Za-z0-9_-]` with `_`, as Codex requires.
fn sanitize_tool_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Makes a tool name safe for Codex and fits it within the limit. Long MCP
/// names (`mcp__server__tool`) keep the `mcp__` prefix and the tool part.
fn shorten_name_if_needed(name: &str) -> String {
    let mut sanitized = sanitize_tool_name(name);
    if sanitized.len() <= NAME_LIMIT {
        return sanitized;
    }
    // The name is ASCII now, so it can be cut at any byte.
    if sanitized.starts_with("mcp__")
        && let Some(index) = sanitized.rfind("__").filter(|&index| index > 0)
    {
        let mut candidate = format!("mcp__{}", &sanitized[index + 2..]);
        candidate.truncate(NAME_LIMIT);
        return candidate;
    }
    sanitized.truncate(NAME_LIMIT);
    sanitized
}

/// Every tool name the request uses, in order: declared tools, then
/// `tool_choice`, then the assistant messages' tool calls.
pub(super) fn collect_request_tool_names(request: &Value) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |name: Cow<'_, str>| {
        if !name.is_empty() && seen.insert(name.clone().into_owned()) {
            names.push(name.into_owned());
        }
    };

    if let Some(Value::Array(tools)) = request.get("tools") {
        for tool in tools {
            match &*str_of(tool.get("type")) {
                "function" => add(str_of(path(tool, "function.name"))),
                "custom" => add(str_of(tool.get("name"))),
                _ => {}
            }
        }
    }

    if let Some(choice @ Value::Object(_)) = request.get("tool_choice") {
        match &*str_of(choice.get("type")) {
            "function" => {
                let name = str_of(path(choice, "function.name"));
                add(if name.is_empty() {
                    str_of(choice.get("name"))
                } else {
                    name
                });
            }
            "custom" => add(str_of(choice.get("name"))),
            _ => {}
        }
    }

    if let Some(Value::Array(messages)) = request.get("messages") {
        for message in messages {
            if str_of(message.get("role")) != "assistant" {
                continue;
            }
            let Some(Value::Array(calls)) = message.get("tool_calls") else {
                continue;
            };
            for call in calls {
                let name = str_of(path(call, "function.name"));
                add(if name.is_empty() {
                    str_of(path(call, "custom.name"))
                } else {
                    name
                });
            }
        }
    }
    names
}

/// Gives each name a unique Codex name within the limit, adding `_1`, `_2`
/// and so on when two shorten to the same one.
pub(super) fn build_short_name_map(names: &[String]) -> ToolNameMap {
    let mut unique = UniqueNames::default();
    let mut map = ToolNameMap::new();
    for name in names {
        map.insert(
            name.clone(),
            unique.claim(&shorten_name_if_needed(name), NAME_LIMIT),
        );
    }
    map
}

#[cfg(test)]
mod tests;
