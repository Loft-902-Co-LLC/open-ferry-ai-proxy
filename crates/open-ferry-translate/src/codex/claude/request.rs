// Ported from CLIProxyAPI internal/translator/codex/claude/codex_claude_request.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Claude Messages request → Codex (OpenAI Responses) request.
//!
//! Deviations from upstream:
//! - Tool parameter schemas keep the client's key order. Upstream round-trips
//!   them through a Go map, which sorts keys alphabetically.
//! - Where upstream copies the client's raw JSON into a string, we write compact
//!   re-serialized JSON. The values are the same JSON. This applies to
//!   `function_call.arguments`, a `function_call_output.output` that falls back
//!   to raw content, and a non-string `text` wrapped in a system reminder.
//! - Names and call IDs cut to 64 bytes are cut at a UTF-8 character boundary.
//!   Upstream slices bytes and can split a character.
//! - Grok reasoning signatures are not replayed to Grok-named models, and the
//!   compatibility variant (`ConvertClaudeRequestToCodexWithCompat`) is not ported.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::claude::{align_tool_results, is_attribution_system_text, message_system_reminder_text};
use crate::go;
use crate::json::{bool_of, int_of, object, str_of};
use crate::schema::{MAP_KEYWORDS, VALUE_KEYWORDS, has_unsupported_unicode_property_escape};
use crate::signature::compatible_gpt_signature;
use crate::thinking::{LEVEL_XHIGH, budget_to_level};

/// The Responses API limit on function names and call IDs.
const ID_LIMIT: usize = 64;
const DEFAULT_REASONING_EFFORT: &str = "medium";
const DEFAULT_STRUCTURED_OUTPUT_NAME: &str = "cli_proxy_structured_output";

pub(super) type ToolNameMap = HashMap<String, String>;

/// Converts a Claude Messages request body into a Codex Responses request body
/// for `model_name`. Codex requests always stream and are never stored.
pub fn convert_claude_request_to_codex(model_name: &str, request: &Value) -> Value {
    let tool_names = build_tool_name_map(request.get("tools"));

    let mut input = Vec::new();
    input.extend(convert_system(request.get("system")));
    if let Some(Value::Array(messages)) = request.get("messages") {
        let mut builder = InputBuilder::new(&tool_names);
        for message in messages {
            builder.push_message(message);
        }
        input.extend(builder.finish());
    }

    let mut out = Map::new();
    out.insert("model".into(), model_name.into());
    out.insert("instructions".into(), "".into());
    out.insert("input".into(), input.into());

    let tools = match request.get("tools") {
        Some(Value::Array(tools)) => Some(tools),
        _ => None,
    };
    if let Some(tools) = tools {
        let web_search_names = web_search_tool_names(tools);
        let choice =
            convert_tool_choice(request.get("tool_choice"), &tool_names, &web_search_names);
        out.insert("tool_choice".into(), choice);
    }

    let disable_parallel = request
        .get("tool_choice")
        .and_then(|choice| choice.get("disable_parallel_tool_use"));
    out.insert(
        "parallel_tool_calls".into(),
        (!disable_parallel.is_some_and(bool_of)).into(),
    );
    // Reasoning summaries are opt-in on the Responses API, so only the effort is set.
    out.insert(
        "reasoning".into(),
        object([("effort", reasoning_effort(request).into())]),
    );
    if let Some(tier) = service_tier(request) {
        out.insert("service_tier".into(), tier.into());
    }
    out.insert("stream".into(), true.into());
    out.insert("store".into(), false.into());
    out.insert("include".into(), json!(["reasoning.encrypted_content"]));
    if let Some(format) = text_format(request) {
        out.insert("text".into(), object([("format", format)]));
    }
    if let Some(tools) = tools {
        let tools: Vec<Value> = tools
            .iter()
            .map(|tool| convert_tool(tool, &tool_names))
            .collect();
        out.insert("tools".into(), tools.into());
    }
    Value::Object(out)
}

/// Turns the top-level `system` prompt into a single developer message.
fn convert_system(system: Option<&Value>) -> Option<Value> {
    let texts: Vec<Cow<'_, str>> = match system? {
        Value::String(text) => vec![Cow::Borrowed(text.as_str())],
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| str_of(block.get("type")) == "text")
            .map(|block| str_of(block.get("text")))
            .collect(),
        _ => return None,
    };
    let content: Vec<Value> = texts
        .into_iter()
        .filter(|text| !text.is_empty() && !is_attribution_system_text(text))
        .map(|text| object([("type", "input_text".into()), ("text", text.into())]))
        .collect();
    if content.is_empty() {
        return None;
    }
    Some(message_item("developer", content))
}

fn message_item(role: &str, content: Vec<Value>) -> Value {
    object([
        ("type", "message".into()),
        ("role", role.into()),
        ("content", content.into()),
    ])
}

/// Builds the Responses `input` list from Claude messages.
///
/// Claude keeps a turn's text, reasoning and tool calls in one message, while
/// the Responses API needs reasoning items and function calls as separate
/// top-level items. Content parts are buffered and flushed into a message item
/// whenever a non-message item has to be emitted, which keeps the original order.
struct InputBuilder<'a> {
    tool_names: &'a ToolNameMap,
    items: Vec<Value>,
    /// IDs of the `tool_use` parts in the latest message, used to put the
    /// following `tool_result` parts in the same order.
    pending_tool_use_ids: Vec<String>,
    /// Mid-conversation system messages that arrived between a tool call and
    /// its result. They are held back so the call and result stay adjacent.
    pending_reminders: Vec<Value>,
}

/// Content parts of the message being converted, not yet emitted as an item.
struct PendingMessage<'r> {
    role: &'r str,
    content: Vec<Value>,
}

impl<'a> InputBuilder<'a> {
    fn new(tool_names: &'a ToolNameMap) -> Self {
        Self {
            tool_names,
            items: Vec::new(),
            pending_tool_use_ids: Vec::new(),
            pending_reminders: Vec::new(),
        }
    }

    fn finish(mut self) -> Vec<Value> {
        self.flush_reminders();
        self.items
    }

    fn push_message(&mut self, message: &Value) {
        let role = str_of(message.get("role"));
        if role == "system" {
            if let Some(text) = message_system_reminder_text(message.get("content")) {
                let reminder = message_item(
                    "user",
                    vec![object([
                        ("type", "input_text".into()),
                        ("text", text.into()),
                    ])],
                );
                if self.pending_tool_use_ids.is_empty() {
                    self.items.push(reminder);
                } else {
                    self.pending_reminders.push(reminder);
                }
            }
            return;
        }

        let tool_use_ids = std::mem::take(&mut self.pending_tool_use_ids);
        let mut pending = PendingMessage {
            role: &role,
            content: Vec::new(),
        };
        match message.get("content") {
            Some(Value::Array(parts)) => {
                let parts = if role == "user" {
                    align_tool_results(parts, &tool_use_ids)
                } else {
                    Cow::Borrowed(parts.as_slice())
                };
                for part in parts.iter() {
                    self.push_part(&mut pending, part);
                }
            }
            Some(Value::String(text)) => pending.push_text(text),
            _ => return,
        }
        self.flush(&mut pending);
        self.flush_reminders();
    }

    fn push_part(&mut self, message: &mut PendingMessage<'_>, part: &Value) {
        match &*str_of(part.get("type")) {
            "text" => {
                self.flush_reminders();
                message.push_text(&str_of(part.get("text")));
            }
            "thinking" => {
                // Only GPT reasoning can be replayed to Codex. The visible
                // thinking text is a summary and is never sent back.
                if message.role != "assistant" {
                    return;
                }
                let signature = str_of(part.get("signature"));
                let Some(signature) = compatible_gpt_signature(&signature) else {
                    return;
                };
                self.flush(message);
                self.items.push(object([
                    ("type", "reasoning".into()),
                    ("summary", json!([])),
                    ("content", Value::Null),
                    ("encrypted_content", signature.into()),
                ]));
            }
            "image" => {
                self.flush_reminders();
                if let Some(url) = image_data_url(part.get("source")) {
                    message.content.push(input_image(url));
                }
            }
            "document" => {
                self.flush_reminders();
                if let Some(url) = pdf_data_url(part.get("source")) {
                    message.content.push(object([
                        ("type", "input_file".into()),
                        ("file_data", url.into()),
                        ("filename", "document.pdf".into()),
                    ]));
                }
            }
            "tool_use" => {
                self.flush(message);
                let id = str_of(part.get("id"));
                if !id.is_empty() {
                    self.pending_tool_use_ids.push(id.to_string());
                }
                let name = codex_tool_name(self.tool_names, &str_of(part.get("name")));
                let arguments = part.get("input").map(Value::to_string).unwrap_or_default();
                self.items.push(object([
                    ("type", "function_call".into()),
                    ("call_id", shorten_call_id(&id).into()),
                    ("name", name.into()),
                    ("arguments", arguments.into()),
                ]));
            }
            "tool_result" => {
                self.flush(message);
                let call_id = shorten_call_id(&str_of(part.get("tool_use_id"))).into_owned();
                self.items.push(object([
                    ("type", "function_call_output".into()),
                    ("call_id", call_id.into()),
                    ("output", tool_result_output(part.get("content"))),
                ]));
            }
            _ => {}
        }
    }

    fn flush(&mut self, message: &mut PendingMessage<'_>) {
        if !message.content.is_empty() {
            let content = std::mem::take(&mut message.content);
            self.items.push(message_item(message.role, content));
        }
    }

    fn flush_reminders(&mut self) {
        self.items.append(&mut self.pending_reminders);
    }
}

impl PendingMessage<'_> {
    fn push_text(&mut self, text: &str) {
        let part_type = if self.role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };
        self.content
            .push(object([("type", part_type.into()), ("text", text.into())]));
    }
}

/// Converts `tool_result` content. Text and image blocks become a content list;
/// anything else is passed on as a string.
fn tool_result_output(content: Option<&Value>) -> Value {
    if let Some(Value::Array(blocks)) = content {
        let items: Vec<Value> = blocks
            .iter()
            .filter_map(|block| match &*str_of(block.get("type")) {
                "image" => image_data_url(block.get("source")).map(input_image),
                "text" => Some(object([
                    ("type", "input_text".into()),
                    ("text", str_of(block.get("text")).into()),
                ])),
                _ => None,
            })
            .collect();
        if !items.is_empty() {
            return items.into();
        }
    }
    str_of(content).into()
}

fn input_image(url: String) -> Value {
    object([("type", "input_image".into()), ("image_url", url.into())])
}

fn image_data_url(source: Option<&Value>) -> Option<String> {
    let source = source?;
    let data = first_non_empty(source, &["data", "base64"])?;
    let media_type = first_non_empty(source, &["media_type", "mime_type"]);
    let media_type = media_type.as_deref().unwrap_or("application/octet-stream");
    Some(format!("data:{media_type};base64,{data}"))
}

/// Only base64 PDFs are forwarded; Codex has no equivalent for other documents.
fn pdf_data_url(source: Option<&Value>) -> Option<String> {
    let source = source?;
    if str_of(source.get("type")) != "base64" {
        return None;
    }
    let media_type = str_of(source.get("media_type"));
    let media_type = media_type.trim();
    if !media_type.eq_ignore_ascii_case("application/pdf") {
        return None;
    }
    let data = first_non_empty(source, &["data", "base64"])?;
    Some(format!("data:{media_type};base64,{data}"))
}

fn first_non_empty<'v>(object: &'v Value, keys: &[&str]) -> Option<Cow<'v, str>> {
    keys.iter()
        .map(|key| str_of(object.get(*key)))
        .find(|value| !value.is_empty())
}

fn is_web_search_tool_type(tool_type: &str) -> bool {
    matches!(tool_type, "web_search_20250305" | "web_search_20260209")
}

/// Names the client gave its server-side web search tools.
fn web_search_tool_names(tools: &[Value]) -> HashSet<Cow<'_, str>> {
    tools
        .iter()
        .filter(|tool| is_web_search_tool_type(&str_of(tool.get("type"))))
        .map(|tool| str_of(tool.get("name")))
        .filter(|name| !name.is_empty())
        .collect()
}

fn convert_tool(tool: &Value, tool_names: &ToolNameMap) -> Value {
    if is_web_search_tool_type(&str_of(tool.get("type"))) {
        return convert_web_search_tool(tool);
    }

    // Upstream edits each tool with sjson, which cannot set keys on an array,
    // so an array passes through unchanged.
    if tool.is_array() {
        return tool.clone();
    }
    let mut out = tool.as_object().cloned().unwrap_or_default();
    if tool.get("type").and_then(Value::as_str) != Some("function") {
        out.insert("type".into(), "function".into());
    }
    if let Some(name) = tool.get("name") {
        let original = str_of(Some(name));
        let codex_name = codex_tool_name(tool_names, &original);
        if !name.is_string() || codex_name != original {
            out.insert("name".into(), codex_name.into());
        }
    }
    out.insert(
        "parameters".into(),
        normalize_tool_parameters(tool.get("input_schema")),
    );
    for key in ["input_schema", "cache_control", "defer_loading"] {
        out.shift_remove(key);
    }
    if out.get("strict") != Some(&Value::Bool(false)) {
        out.insert("strict".into(), false.into());
    }
    Value::Object(out)
}

/// Maps Claude's server-side web search tool onto the Responses built-in one.
/// Codex has no blocklist, so `blocked_domains` is dropped.
fn convert_web_search_tool(tool: &Value) -> Value {
    let mut out = json!({ "type": "web_search" });
    if let Some(domains @ Value::Array(_)) = tool.get("allowed_domains") {
        out["filters"] = json!({ "allowed_domains": domains });
    }
    if let Some(location @ Value::Object(_)) = tool.get("user_location") {
        out["user_location"] = location.clone();
    }
    out
}

fn convert_tool_choice(
    choice: Option<&Value>,
    tool_names: &ToolNameMap,
    web_search_names: &HashSet<Cow<'_, str>>,
) -> Value {
    let Some(choice) = choice.filter(|choice| !choice.is_null()) else {
        return json!("auto");
    };
    let mut choice_type = str_of(choice.get("type"));
    if choice_type.is_empty()
        && let Value::String(mode) = choice
    {
        choice_type = Cow::Borrowed(mode);
    }

    match &*choice_type {
        "any" => json!("required"),
        "none" => json!("none"),
        "tool" => {
            let name = str_of(choice.get("name"));
            if web_search_names.contains(&*name) {
                return json!({ "type": "web_search" });
            }
            let name = codex_tool_name(tool_names, &name);
            if name.is_empty() {
                return json!("auto");
            }
            object([("type", "function".into()), ("name", name.into())])
        }
        _ => json!("auto"),
    }
}

/// Claude thinking settings → Responses `reasoning.effort`.
fn reasoning_effort(request: &Value) -> String {
    let Some(thinking @ Value::Object(_)) = request.get("thinking") else {
        return DEFAULT_REASONING_EFFORT.into();
    };
    let level = match &*str_of(thinking.get("type")) {
        "enabled" => thinking
            .get("budget_tokens")
            .and_then(|budget| budget_to_level(int_of(budget))),
        "adaptive" | "auto" => {
            // Adaptive thinking may carry an explicit effort (Claude 4.6+).
            let effort = request
                .get("output_config")
                .and_then(|config| config.get("effort"))
                .and_then(Value::as_str)
                .map(|effort| go::to_lower(effort.trim()))
                .filter(|effort| !effort.is_empty());
            return effort.unwrap_or_else(|| LEVEL_XHIGH.into());
        }
        "disabled" => budget_to_level(0),
        _ => None,
    };
    level.unwrap_or(DEFAULT_REASONING_EFFORT).into()
}

fn service_tier(request: &Value) -> Option<&'static str> {
    if request.get("speed").and_then(Value::as_str) == Some("fast") {
        return Some("priority");
    }
    let tier = request.get("service_tier").and_then(Value::as_str)?;
    matches!(go::to_lower(tier.trim()).as_str(), "fast" | "priority").then_some("priority")
}

/// Claude `output_config.format` → Responses `text.format`.
fn text_format(request: &Value) -> Option<Value> {
    let format = request.get("output_config")?.get("format")?;
    if !format.is_object() || str_of(format.get("type")) != "json_schema" {
        return None;
    }
    let schema = format.get("schema").filter(|schema| schema.is_object())?;
    let name = str_of(format.get("name"));
    let name = if name.is_empty() {
        DEFAULT_STRUCTURED_OUTPUT_NAME
    } else {
        &name
    };
    // Strict mode rejects schemas with optional properties (HTTP 400), so
    // downgrade instead of sending a schema the upstream cannot satisfy.
    let strict =
        format.get("strict") != Some(&Value::Bool(false)) && !schema_misses_required(schema);
    Some(json!({ "type": "json_schema", "name": name, "strict": strict, "schema": schema }))
}

/// Reports whether any (sub)schema declares a property its `required` list omits.
fn schema_misses_required(schema: &Value) -> bool {
    let map = match schema {
        Value::Object(map) => map,
        Value::Array(children) => return children.iter().any(schema_misses_required),
        _ => return false,
    };
    if let Some(Value::Object(properties)) = map.get("properties") {
        let Some(Value::Array(required)) = map.get("required") else {
            // As upstream: no `required` list settles it without checking subschemas.
            return !properties.is_empty();
        };
        let required: HashSet<&str> = required.iter().filter_map(Value::as_str).collect();
        if properties
            .keys()
            .any(|name| !required.contains(name.as_str()))
        {
            return true;
        }
    }
    let in_maps = MAP_KEYWORDS
        .iter()
        .filter_map(|keyword| map.get(*keyword).and_then(Value::as_object))
        .any(|children| children.values().any(schema_misses_required));
    in_maps
        || VALUE_KEYWORDS
            .iter()
            .filter_map(|keyword| map.get(*keyword))
            .any(schema_misses_required)
}

/// Makes a Claude `input_schema` acceptable as Responses function `parameters`:
/// object schemas get a `properties` map, dialect keywords are dropped, and so
/// are regexes the upstream validator cannot compile.
fn normalize_tool_parameters(schema: Option<&Value>) -> Value {
    let Some(Value::Object(root)) = schema else {
        return json!({ "type": "object", "properties": {} });
    };
    let mut root = root.clone();
    strip_dialect_keywords_from_object(&mut root);

    let untyped = root
        .get("type")
        .is_none_or(|t| t.is_null() || t.as_str() == Some(""));
    if untyped {
        root.insert("type".into(), "object".into());
    }
    let is_object = match root.get("type") {
        Some(Value::String(schema_type)) => schema_type == "object",
        Some(Value::Array(types)) => types.iter().any(|t| t.as_str() == Some("object")),
        _ => false,
    };
    if is_object && root.get("properties").is_none_or(Value::is_null) {
        root.insert("properties".into(), json!({}));
    }
    Value::Object(root)
}

fn strip_dialect_keywords(schema: &mut Value) {
    match schema {
        Value::Object(map) => strip_dialect_keywords_from_object(map),
        Value::Array(items) => items.iter_mut().for_each(strip_dialect_keywords),
        _ => {}
    }
}

/// Removes `$schema`, `$id` and unsupported regexes from a schema and every
/// subschema. Only schema positions are visited, so property *names* such as
/// `"$id"` and literal data under `default`, `const` or `enum` are left alone.
fn strip_dialect_keywords_from_object(schema: &mut Map<String, Value>) {
    schema.shift_remove("$schema");
    schema.shift_remove("$id");
    if schema
        .get("pattern")
        .and_then(Value::as_str)
        .is_some_and(has_unsupported_unicode_property_escape)
    {
        schema.shift_remove("pattern");
    }

    for keyword in MAP_KEYWORDS {
        let Some(Value::Object(children)) = schema.get_mut(keyword) else {
            continue;
        };
        if keyword == "patternProperties" {
            children.retain(|pattern, _| !has_unsupported_unicode_property_escape(pattern));
        }
        children.values_mut().for_each(strip_dialect_keywords);
    }
    for keyword in VALUE_KEYWORDS {
        if let Some(child) = schema.get_mut(keyword) {
            strip_dialect_keywords(child);
        }
    }
}

/// Maps each declared tool name to a unique name within the Responses limit.
/// When two tools share a name, both end up with the later one's suffixed name,
/// as upstream does.
pub(super) fn build_tool_name_map(tools: Option<&Value>) -> ToolNameMap {
    let mut map = ToolNameMap::new();
    let Some(Value::Array(tools)) = tools else {
        return map;
    };
    let mut used = HashSet::new();
    for tool in tools {
        let name = str_of(tool.get("name"));
        if name.is_empty() {
            continue;
        }
        let unique = unique_name(&shorten_name(&name), &used);
        used.insert(unique.clone());
        map.insert(name.into_owned(), unique);
    }
    map
}

fn codex_tool_name(tool_names: &ToolNameMap, name: &str) -> String {
    match tool_names.get(name) {
        Some(short) => short.clone(),
        None => shorten_name(name).into_owned(),
    }
}

/// Fits a tool name within the limit. Long MCP names (`mcp__server__tool`) keep
/// the `mcp__` prefix and the tool part, since the server part is the long one.
fn shorten_name(name: &str) -> Cow<'_, str> {
    if name.len() <= ID_LIMIT {
        return Cow::Borrowed(name);
    }
    if name.starts_with("mcp__")
        && let Some(index) = name.rfind("__")
    {
        let candidate = format!("mcp__{}", &name[index + 2..]);
        return Cow::Owned(truncate_bytes(&candidate, ID_LIMIT).to_owned());
    }
    Cow::Borrowed(truncate_bytes(name, ID_LIMIT))
}

fn unique_name(candidate: &str, used: &HashSet<String>) -> String {
    if !used.contains(candidate) {
        return candidate.to_owned();
    }
    let mut n = 1usize;
    loop {
        let suffix = format!("_{n}");
        let prefix = truncate_bytes(candidate, ID_LIMIT.saturating_sub(suffix.len()));
        let unique = format!("{prefix}{suffix}");
        if !used.contains(&unique) {
            return unique;
        }
        n += 1;
    }
}

/// Keeps Claude tool IDs within the Responses `call_id` limit. A long ID becomes
/// its first 47 bytes, `_`, and 16 hex digits of its SHA-256, so the mapping is
/// stable across turns and a tool call still matches its result.
pub(super) fn shorten_call_id(id: &str) -> Cow<'_, str> {
    if id.len() <= ID_LIMIT {
        return Cow::Borrowed(id);
    }
    let digest = Sha256::digest(id.as_bytes());
    let mut short = truncate_bytes(id, ID_LIMIT - 17).to_owned();
    short.push('_');
    for byte in &digest[..8] {
        write!(short, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Cow::Owned(short)
}

/// Cuts `s` to at most `max` bytes without splitting a character.
fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests;
