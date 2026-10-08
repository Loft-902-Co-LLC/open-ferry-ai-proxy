// Ported from CLIProxyAPI internal/translator/claude/openai/responses/claude_openai-responses_request.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! OpenAI Responses request → Claude Messages request.
//!
//! `instructions` and system or developer input items become Claude's
//! top-level `system` blocks, in order. The other input items become
//! alternating user and assistant turns:
//! - messages become text, image and document blocks;
//! - function and custom tool calls become `tool_use` blocks;
//! - their outputs become `tool_result` blocks;
//! - reasoning items with a Claude signature become thinking blocks;
//! - web searches become Claude's server tool pair.
//!
//! The history is then repaired to Claude's rules:
//! - every `tool_use` is answered at the start of the next user turn;
//! - a trailing thinking block is dropped;
//! - a trailing assistant turn is dropped for models that reject prefill.
//!
//! Tools declared at the top level, in `additional_tools` items and in
//! namespaces are named for Claude as [`super::tools`] describes.
//!
//! A user message's `input_file` part without `file_data`, or `input_audio`
//! part, has no Claude block. The rest of the message is still sent, but a
//! user message left with nothing is refused with an
//! [`UnsupportedPartError`].
//!
//! Deviations from upstream:
//! - `metadata.user_id` is only set to an ID the client sent, in
//!   `metadata.user_id` or `user`. Without one, upstream derives an ID from
//!   the conversation; we don't make up user IDs.
//! - A tool call without an ID gets one in upstream's form, `toolu_` and 24
//!   letters and digits, drawn from the standard library's randomly keyed
//!   hasher rather than the operating system's random source.
//! - Where upstream copies the client's JSON text, we write the same JSON
//!   compactly. This applies to:
//!   - function call arguments;
//!   - tools of types we don't convert;
//!   - a web search's allowed domains and user location;
//!   - citations and search results;
//!   - a tool output that has no part Claude can carry, which is sent as
//!     text;
//!   - a non-string value read as text.
//! - JSON that serde_json can't read is not read. This covers function call
//!   arguments nested more than 128 levels deep or with a lone UTF-16
//!   surrogate escape, which give `{}`.
//! - When an object repeats a key, the last value counts; gjson reads the
//!   first.
//! - Input items of types with no Claude counterpart are dropped without the
//!   warning upstream logs, and so are upstream's warnings about histories
//!   the repair can't fix.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use super::tools::{
    Descriptor, RequestTools, allows_external_web_access, is_unsupported_builtin_tool_type,
    qualify, tool_description, tool_name, tool_parameters,
};
use super::web_search::{
    attach_claude_citations, convert_responses_web_search_call_to_claude_blocks,
};
use crate::apply_patch;
use crate::common::cache_control;
use crate::common::claude::{
    apply_reasoning_effort, client_user_id, generate_tool_call_id, sanitize_function_name,
    sanitize_tool_id, structured_output_instruction,
};
use crate::common::parts::UserTurnDrops;
use crate::common::responses::{extract_responses_call_id, normalize_responses_tool_call_outputs};
use crate::go;
use crate::json::lenient::{self, Found};
use crate::json::{int_of, object, path, str_of};
use crate::models::ModelCatalog;
use crate::registry::UnsupportedPartError;
use crate::schema::normalize_claude_tool_input_schema;
use crate::signature::{Provider, compatible_signature_for_provider};
use crate::thinking::summary::apply_translated_to_claude;

const DEFAULT_MAX_TOKENS: i64 = 32000;
const DEFAULT_FABLE_MAX_TOKENS: i64 = 64000;

/// `ClaudeResponsesRedactedThinkingPrefix`: marks a reasoning item's
/// `encrypted_content` as the data of a Claude `redacted_thinking` block.
pub(super) const REDACTED_THINKING_PREFIX: &str = "claude-redacted-thinking:";

/// The text of a tool result that was lost when its call was cut short.
const INTERRUPTED_TOOL_RESULT: &str = "Tool call was interrupted before any output was recorded.";

/// Stands in for a tool output with nothing to show, which Claude would
/// reject as an empty block.
const EMPTY_TOOL_RESULT: &str = "Tool result was empty.";

/// Converts a Responses request body into a Claude Messages request body for
/// `model_name`. `stream` is whether the client asked to stream. `models`
/// says which thinking settings and output limit the model has; pass
/// [`ModelCatalog::current`] unless you have your own.
///
/// The error is set when a user message had only parts Claude can't take;
/// the body is still returned.
pub fn convert_openai_responses_request_to_claude(
    model_name: &str,
    request: &Value,
    stream: bool,
    models: &ModelCatalog,
) -> (Value, Option<UnsupportedPartError>) {
    convert(model_name, request, stream, models, false)
}

/// [`convert_openai_responses_request_to_claude`] for compatibility
/// endpoints. A reasoning item whose signature isn't Claude's is replayed
/// with it as it is, rather than dropped, and the history keeps a trailing
/// thinking block or assistant turn.
pub fn convert_openai_responses_request_to_claude_with_compat(
    model_name: &str,
    request: &Value,
    stream: bool,
    models: &ModelCatalog,
) -> (Value, Option<UnsupportedPartError>) {
    convert(model_name, request, stream, models, true)
}

fn convert(
    model_name: &str,
    request: &Value,
    stream: bool,
    models: &ModelCatalog,
    compat: bool,
) -> (Value, Option<UnsupportedPartError>) {
    let request = normalize_codex_agent_messages(request);
    let request = request.as_ref();

    let mut out = Map::new();
    out.insert("model".into(), "".into());
    out.insert(
        "max_tokens".into(),
        default_max_tokens(model_name, models).into(),
    );
    out.insert("messages".into(), json!([]));
    let metadata = match client_user_id(request) {
        Some(user_id) => object([("user_id", user_id.into())]),
        None => json!({}),
    };
    out.insert("metadata".into(), metadata);

    if let Some(effort) = path(request, "reasoning.effort") {
        apply_reasoning_effort(&mut out, &str_of(Some(effort)), model_name, models);
    }
    out.insert("model".into(), model_name.into());
    if let Some(max_tokens) = request.get("max_output_tokens").filter(|v| !v.is_null()) {
        let mut max_tokens = int_of(max_tokens);
        if let Some(limit) = max_completion_tokens(model_name, models)
            && max_tokens > limit
        {
            max_tokens = limit;
        }
        out.insert("max_tokens".into(), max_tokens.into());
    }
    out.insert("stream".into(), stream.into());
    if request.get("service_tier").and_then(Value::as_str) == Some("priority") {
        out.insert("speed".into(), "fast".into());
    }

    // Each source block stays a block of its own, in order. Where they end up
    // is the Claude executor's call, so they aren't merged or turned into user
    // text here.
    let mut system = Vec::new();
    if let Some(Value::String(instructions)) = request.get("instructions") {
        push_system_text(&mut system, instructions, None);
    }
    let input = request.get("input");
    if let Some(Value::Array(items)) = input {
        for item in items {
            if is_system_level_role(&str_of(item.get("role"))) {
                push_system_item(&mut system, item);
            }
        }
    }
    let format = path(request, "text.format").or_else(|| request.get("response_format"));
    if let Some(instruction) = structured_output_instruction(format) {
        push_system_text(&mut system, &instruction, None);
    }

    let tools = RequestTools::new(request);
    let mut drops = UserTurnDrops::default();
    let mut messages = convert_input(input, &tools, &mut drops, compat);
    let had_messages = !messages.is_empty();
    if !compat {
        strip_trailing_thinking(&mut messages);
    }
    // Before the prefill check, so an interrupted turn ends with a made-up
    // tool result rather than an assistant turn some models reject.
    let mut messages = repair_tool_pairing(messages);
    if !compat && rejects_assistant_prefill(model_name) && messages.last().is_some_and(is_assistant)
    {
        messages.pop();
    }
    // A request of only system input, or whose turns were all dropped, still
    // needs a turn.
    if messages.is_empty() && (!system.is_empty() || had_messages) {
        messages.push(json!({"role": "user", "content": [{"type": "text", "text": ""}]}));
    }
    out.insert("messages".into(), messages.into());
    if !system.is_empty() {
        out.insert("system".into(), system.into());
    }

    let mut included = HashSet::new();
    let mut claude_tools = Vec::new();
    for descriptor in tools.winning() {
        let Some(tool) = convert_tool(descriptor, &tools.claude_name(&descriptor.name)) else {
            continue;
        };
        let name = str_of(tool.get("name")).into_owned();
        if !name.is_empty() {
            included.insert(descriptor.name.clone());
            included.insert(name);
        }
        claude_tools.push(tool);
    }
    if !claude_tools.is_empty() {
        out.insert("tools".into(), claude_tools.into());
    }
    let name_map = tools.name_map(&included);
    if let Some(choice) =
        convert_tool_choice(request.get("tool_choice"), &tools, &included, &name_map)
    {
        out.insert("tool_choice".into(), choice);
    }

    let mut out = Value::Object(out);
    apply_translated_to_claude(&mut out, request, "openai-response", model_name, models);
    (out, drops.err())
}

/// `defaultClaudeResponsesMaxTokensForModel`: 64000 for Fable models and
/// 32000 for others, or the model's own limit if that is lower.
fn default_max_tokens(model_name: &str, models: &ModelCatalog) -> i64 {
    let default = if go::to_lower(model_name.trim()).contains("fable") {
        DEFAULT_FABLE_MAX_TOKENS
    } else {
        DEFAULT_MAX_TOKENS
    };
    match max_completion_tokens(model_name, models) {
        Some(limit) if limit < default => limit,
        _ => default,
    }
}

/// The most output tokens the catalog says the model takes, if it says.
fn max_completion_tokens(model_name: &str, models: &ModelCatalog) -> Option<i64> {
    models
        .lookup(model_name)
        .map(|model| model.max_completion_tokens)
        .filter(|&limit| limit > 0)
}

/// `normalizeCodexAgentMessages`: Codex's multi-agent `agent_message` items
/// become user messages, and their encrypted content parts input text, so the
/// delegated task isn't dropped.
fn normalize_codex_agent_messages(request: &Value) -> Cow<'_, Value> {
    let is_agent_message = |item: &Value| str_of(item.get("type")).trim() == "agent_message";
    match request.get("input") {
        Some(Value::Array(items)) if items.iter().any(is_agent_message) => {}
        _ => return Cow::Borrowed(request),
    }
    let mut request = request.clone();
    if let Some(Value::Array(items)) = request.get_mut("input") {
        for item in items.iter_mut().filter(|item| is_agent_message(item)) {
            let Value::Object(item) = item else {
                continue;
            };
            if let Some(Value::Array(parts)) = item.get_mut("content") {
                for part in parts {
                    if str_of(part.get("type")).trim() != "encrypted_content" {
                        continue;
                    }
                    let Some(Value::String(encrypted)) = part.get("encrypted_content") else {
                        continue;
                    };
                    let encrypted = encrypted.clone();
                    if let Value::Object(part) = part {
                        part.insert("type".into(), "input_text".into());
                        part.insert("text".into(), encrypted.into());
                        part.shift_remove("encrypted_content");
                    }
                }
            }
            item.insert("role".into(), "user".into());
            item.insert("type".into(), "message".into());
        }
    }
    Cow::Owned(request)
}

/// `isResponsesSystemLevelRole`: the Responses API ranks system and developer
/// input above the user's, so both go in Claude's system slot.
fn is_system_level_role(role: &str) -> bool {
    matches!(go::to_lower(role.trim()).as_str(), "system" | "developer")
}

/// `appendSystemText`: a system text block, with `source`'s cache marker.
/// Empty text gives none.
fn push_system_text(system: &mut Vec<Value>, text: &str, source: Option<&Value>) {
    if text.is_empty() {
        return;
    }
    let mut block = text_block(text);
    if let Some(source) = source {
        cache_control::attach_to(&mut block, source);
    }
    system.push(block);
}

/// The system blocks of a system or developer input item.
fn push_system_item(system: &mut Vec<Value>, item: &Value) {
    let start = system.len();
    match item.get("content") {
        Some(Value::String(text)) => push_system_text(system, text, None),
        Some(Value::Array(parts)) => {
            for part in parts {
                match &*str_of(part.get("type")) {
                    "input_text" | "output_text" | "text" => {
                        push_system_text(system, &str_of(part.get("text")), Some(part));
                    }
                    // `responsesSystemUnsupportedBlock`: Claude takes only text
                    // here. Anything else becomes a block of just its type, so
                    // the request fails naming it rather than losing the
                    // operator's instructions without a word.
                    kind => {
                        let kind = kind.trim();
                        if !kind.is_empty() {
                            system.push(object([("type", kind.into())]));
                        }
                    }
                }
            }
        }
        _ => {}
    }
    // An item's marker goes on the last block it gave.
    cache_control::attach_to_last_block(&mut system[start..], item);
}

/// Builds Claude turns from input items, merging consecutive items of one
/// role. An assistant's tool calls wait at the end of its turn.
#[derive(Default)]
struct Turns {
    messages: Vec<Value>,
    /// The role of the turn being built; empty when there is none.
    role: &'static str,
    parts: Vec<Value>,
    tool_uses: Vec<Value>,
}

impl Turns {
    /// `flushPendingMessage`: ends the turn being built.
    fn flush(&mut self) {
        if self.role.is_empty() {
            return;
        }
        let mut parts = std::mem::take(&mut self.parts);
        let tool_uses = std::mem::take(&mut self.tool_uses);
        if self.role == "assistant" && !tool_uses.is_empty() {
            if let Some(separator) = thinking_separator_for_tool_use(&parts) {
                parts.push(separator);
            }
            parts.extend(tool_uses);
        }
        if !parts.is_empty() {
            self.messages.push(object([
                ("role", self.role.into()),
                ("content", message_content(parts)),
            ]));
        }
        self.role = "";
    }

    fn start(&mut self, role: &'static str) {
        if !self.role.is_empty() && self.role != role {
            self.flush();
        }
        self.role = role;
    }

    /// `appendParts`.
    fn push(&mut self, role: &'static str, parts: Vec<Value>) {
        if parts.is_empty() {
            return;
        }
        self.start(role);
        self.parts.extend(parts);
    }

    /// `appendToolUse`.
    fn push_tool_use(&mut self, tool_use: Value) {
        self.start("assistant");
        self.tool_uses.push(tool_use);
    }

    /// `appendReasoning`: a reasoning item after tool calls really does
    /// separate thinking blocks, so the calls take their place before it. A
    /// thinking block right after another replaces it.
    fn push_reasoning(&mut self, block: Value) {
        self.start("assistant");
        self.parts.append(&mut self.tool_uses);
        if str_of(block.get("type")) == "thinking"
            && let Some(last) = self.parts.last_mut()
            && str_of(last.get("type")) == "thinking"
        {
            *last = block;
            return;
        }
        self.parts.push(block);
    }
}

/// `claudeThinkingSeparatorForToolUse`: the latest thinking block, when the
/// assistant's content ends with a web search result. Upstreams that enforce
/// Claude's thinking rules reject a `tool_use` right after a server tool
/// result, and Claude's own output always thinks again there.
fn thinking_separator_for_tool_use(parts: &[Value]) -> Option<Value> {
    if str_of(parts.last()?.get("type")) != "web_search_tool_result" {
        return None;
    }
    parts
        .iter()
        .rev()
        .find(|part| str_of(part.get("type")) == "thinking")
        .cloned()
}

/// A message's content: a lone plain text block as a string, otherwise the
/// blocks.
fn message_content(parts: Vec<Value>) -> Value {
    if let [part] = parts.as_slice()
        && str_of(part.get("type")) == "text"
        && part.get("cache_control").is_none()
        && part.get("citations").is_none()
    {
        return str_of(part.get("text")).into_owned().into();
    }
    parts.into()
}

/// The input items as Claude turns. Each user message item's dropped parts
/// go into `drops`.
fn convert_input(
    input: Option<&Value>,
    tools: &RequestTools,
    drops: &mut UserTurnDrops,
    compat: bool,
) -> Vec<Value> {
    let mut turns = Turns::default();
    let items = match input {
        Some(Value::Array(items)) => normalize_responses_tool_call_outputs(items),
        Some(Value::String(text)) => {
            turns.push("user", vec![text_block(text.as_str())]);
            Vec::new()
        }
        _ => Vec::new(),
    };

    // A call answered more than once takes its last answer, in the place of
    // its first.
    let mut last_outputs = HashMap::new();
    for item in &items {
        if is_tool_output(item) {
            let id = extract_responses_call_id(item);
            if !id.is_empty() {
                last_outputs.insert(id, item.as_ref());
            }
        }
    }
    let mut answered = HashSet::new();
    let mut called = HashSet::new();

    for item in &items {
        let item: &Value = item;
        // These already became system blocks.
        if is_system_level_role(&str_of(item.get("role"))) {
            continue;
        }
        let mut kind = str_of(item.get("type"));
        if kind.is_empty() && !str_of(item.get("role")).is_empty() {
            kind = "message".into();
        }
        match &*kind {
            "message" => {
                let (role, parts, dropped) = message_parts(item);
                if role == "user" {
                    if let Some(dropped) = dropped {
                        drops.drop_part(dropped);
                    }
                    drops.end_turn(parts.len());
                }
                turns.push(role, parts);
            }
            "web_search_call" => {
                if let Some(blocks) = convert_responses_web_search_call_to_claude_blocks(item) {
                    turns.push("assistant", blocks.into());
                }
            }
            "reasoning" => {
                if let Some(block) = reasoning_block(item, compat) {
                    turns.push_reasoning(block);
                }
            }
            call @ ("function_call" | "custom_tool_call") => {
                let raw_id = extract_responses_call_id(item);
                let id = if raw_id.is_empty() {
                    generate_tool_call_id()
                } else {
                    sanitize_tool_id(&raw_id)
                };
                // Pairing goes by the raw ID, so two IDs that sanitize alike
                // aren't taken for one call.
                called.insert(raw_id);
                let mut name = str_of(item.get("name")).into_owned();
                let namespace = str_of(item.get("namespace"));
                if !namespace.trim().is_empty() {
                    // The qualified name the previous turn was given.
                    name = qualify(namespace.trim(), &name);
                }
                // Claude wants an object, so custom input is wrapped in one.
                let input = if call == "custom_tool_call" {
                    json!({"input": str_of(item.get("input"))})
                } else {
                    let arguments = str_of(item.get("arguments"));
                    serde_json::from_str::<Value>(&arguments)
                        .ok()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({}))
                };
                turns.push_tool_use(object([
                    ("type", "tool_use".into()),
                    ("id", id.into()),
                    ("name", tools.claude_name(&name).into()),
                    ("input", input),
                ]));
            }
            "function_call_output" | "custom_tool_call_output" => {
                let raw_id = extract_responses_call_id(item);
                if !raw_id.is_empty() && !answered.insert(raw_id.clone()) {
                    continue;
                }
                let output = last_outputs
                    .get(&raw_id)
                    .copied()
                    .unwrap_or(item)
                    .get("output");
                // An output with no call before it, such as the task a
                // delegated thread starts with, has no tool_use to answer.
                // Claude rejects such a tool_result, so it goes as user
                // content instead.
                if raw_id.is_empty() || !called.contains(&raw_id) {
                    turns.push("user", standalone_output_blocks(output));
                    continue;
                }
                turns.push(
                    "user",
                    vec![object([
                        ("type", "tool_result".into()),
                        ("tool_use_id", sanitize_tool_id(&raw_id).into()),
                        ("content", tool_result_content(output)),
                    ])],
                );
            }
            // `additional_tools` items declare tools; other types have no
            // Claude counterpart yet.
            _ => {}
        }
    }
    turns.flush();
    turns.messages
}

fn is_tool_output(item: &Value) -> bool {
    matches!(
        &*str_of(item.get("type")),
        "function_call_output" | "custom_tool_call_output"
    )
}

/// A message item's content as Claude blocks, the role of the turn they go
/// in (the role its parts imply, or else the item's own) and the type of the
/// first part that has no block: an `input_file` without `file_data`, or
/// `input_audio`, which Claude has no block for.
fn message_parts(item: &Value) -> (&'static str, Vec<Value>, Option<&'static str>) {
    let mut role = "";
    let mut parts = Vec::new();
    let mut dropped = None;
    match item.get("content") {
        Some(Value::Array(content)) => {
            for part in content {
                match &*str_of(part.get("type")) {
                    kind @ ("input_text" | "output_text") => {
                        if let Some(text) = part.get("text") {
                            let mut block = Map::new();
                            block.insert("type".into(), "text".into());
                            block.insert("text".into(), str_of(Some(text)).into());
                            attach_claude_citations(&mut block, part.get("annotations"));
                            cache_control::attach(&mut block, part);
                            parts.push(Value::Object(block));
                        }
                        role = if kind == "input_text" {
                            "user"
                        } else {
                            "assistant"
                        };
                    }
                    // Claude has no refusal block; as text, the turn stays
                    // whole.
                    "refusal" => {
                        let refusal = str_of(part.get("refusal"));
                        if !refusal.is_empty() {
                            let mut block = text_block(refusal);
                            cache_control::attach_to(&mut block, part);
                            parts.push(block);
                        }
                        role = "assistant";
                    }
                    kind @ ("input_image" | "input_file") => {
                        if let Some(mut block) = content_part(part) {
                            cache_control::attach_to(&mut block, part);
                            parts.push(block);
                            if role.is_empty() {
                                role = "user";
                            }
                        } else if kind == "input_file" {
                            dropped.get_or_insert("input_file");
                        }
                    }
                    "input_audio" => {
                        dropped.get_or_insert("input_audio");
                    }
                    _ => {}
                }
            }
        }
        Some(Value::String(text)) if !text.is_empty() => parts.push(text_block(text.as_str())),
        _ => {}
    }
    if role.is_empty() {
        role = match &*str_of(item.get("role")) {
            "assistant" => "assistant",
            _ => "user",
        };
    }
    // An item's marker goes on its last block.
    cache_control::attach_to_last_block(&mut parts, item);
    (role, parts, dropped)
}

/// `convertResponsesContentPartToClaude`: a text, image or file part as a
/// Claude block.
fn content_part(part: &Value) -> Option<Value> {
    match &*str_of(part.get("type")) {
        "input_text" | "output_text" => Some(text_block(str_of(Some(part.get("text")?)))),
        "input_image" => {
            let mut url = str_of(part.get("image_url"));
            if url.is_empty() {
                url = str_of(part.get("url"));
            }
            if url.is_empty() {
                return None;
            }
            let Some(rest) = url.strip_prefix("data:") else {
                return Some(json!({"type": "image", "source": {"type": "url", "url": url}}));
            };
            let (media_type, data) = rest.split_once(";base64,").unwrap_or(("", ""));
            if data.is_empty() {
                return None;
            }
            Some(json!({
                "type": "image",
                "source": {"type": "base64", "media_type": or_octet_stream(media_type), "data": data},
            }))
        }
        "input_file" => {
            let file_data = str_of(part.get("file_data"));
            if file_data.is_empty() {
                return None;
            }
            let (media_type, data) = file_data
                .strip_prefix("data:")
                .and_then(|rest| rest.split_once(";base64,"))
                .unwrap_or(("", &file_data));
            Some(json!({
                "type": "document",
                "source": {"type": "base64", "media_type": or_octet_stream(media_type), "data": data},
            }))
        }
        _ => None,
    }
}

fn or_octet_stream(media_type: &str) -> &str {
    if media_type.is_empty() {
        "application/octet-stream"
    } else {
        media_type
    }
}

/// `applyResponsesToolResultContent`: a tool output as a `tool_result`'s
/// content. Parts become Claude blocks, a lone text block becomes a string,
/// and an array with no part Claude can carry is sent as JSON text.
fn tool_result_content(output: Option<&Value>) -> Value {
    let Some(Value::Array(parts)) = output else {
        return str_of(output).into();
    };
    let blocks: Vec<Value> = parts.iter().filter_map(content_part).collect();
    if blocks.is_empty() {
        return output.map(Value::to_string).unwrap_or_default().into();
    }
    if let [block] = blocks.as_slice()
        && str_of(block.get("type")) == "text"
    {
        return str_of(block.get("text")).into_owned().into();
    }
    blocks.into()
}

/// `convertResponsesStandaloneToolOutputToClaudeText`: a tool output with no
/// call to answer, as user content. It never comes out empty, which would
/// leave an empty user turn.
fn standalone_output_blocks(output: Option<&Value>) -> Vec<Value> {
    if let Some(Value::Array(parts)) = output {
        let blocks: Vec<Value> = parts
            .iter()
            .filter_map(content_part)
            .filter(has_visible_content)
            .collect();
        if blocks.is_empty() {
            // Not the array as text: that would only show the empty parts.
            return vec![text_block(EMPTY_TOOL_RESULT)];
        }
        return blocks;
    }
    let text = str_of(output);
    if text.trim().is_empty() {
        return vec![text_block(EMPTY_TOOL_RESULT)];
    }
    vec![text_block(text)]
}

/// `contentPartHasVisibleContent`: text that isn't blank, or any other block.
fn has_visible_content(block: &Value) -> bool {
    str_of(block.get("type")) != "text" || !str_of(block.get("text")).trim().is_empty()
}

/// `convertResponsesReasoningToClaudeThinking`: a reasoning item as the
/// Claude thinking block it came from. Claude needs a signature on every
/// thinking block, so an item without a Claude signature is dropped, except
/// in compatibility mode, which replays the item's own. Claude doesn't check
/// the text against the signature, so the summary can stand in for it.
fn reasoning_block(item: &Value, compat: bool) -> Option<Value> {
    let encrypted = str_of(item.get("encrypted_content"));
    if let Some(data) = encrypted.trim().strip_prefix(REDACTED_THINKING_PREFIX) {
        let data = data.trim();
        if data.is_empty() {
            return None;
        }
        return Some(json!({"type": "redacted_thinking", "data": data}));
    }
    let signature = match compatible_signature_for_provider(Provider::Claude, &encrypted) {
        Some(signature) => signature,
        None if compat => encrypted.into_owned(),
        None => return None,
    };
    Some(object([
        ("type", "thinking".into()),
        ("thinking", reasoning_text(item).into()),
        ("signature", signature.into()),
    ]))
}

/// `responsesReasoningText`: the summary's text, or else the content's.
/// Clients echo the item through whichever array their SDK has; reading both
/// would repeat text mirrored into each.
fn reasoning_text(item: &Value) -> String {
    let text = reasoning_parts_text(item.get("summary"));
    if !text.is_empty() {
        return text;
    }
    reasoning_parts_text(item.get("content"))
}

fn reasoning_parts_text(parts: Option<&Value>) -> String {
    let Some(Value::Array(parts)) = parts else {
        return String::new();
    };
    parts
        .iter()
        .map(|part| match (part.get("text"), part) {
            (Some(text), _) => str_of(Some(text)),
            (None, Value::String(text)) => Cow::Borrowed(text.as_str()),
            (None, _) => Cow::Borrowed(""),
        })
        .collect()
}

fn is_assistant(message: &Value) -> bool {
    go::equal_fold(str_of(message.get("role")).trim(), "assistant")
}

/// A message's content blocks; none if its content is text.
fn blocks(message: &Value) -> &[Value] {
    match message.get("content") {
        Some(Value::Array(blocks)) => blocks,
        _ => &[],
    }
}

fn blocks_of_type<'a>(message: &'a Value, kind: &'a str) -> impl Iterator<Item = &'a Value> {
    blocks(message)
        .iter()
        .filter(move |block| str_of(block.get("type")) == kind)
}

/// `stripTrailingClaudeThinkingBlocks`: Claude rejects a final assistant turn
/// that ends with thinking, so those blocks go, and the turn too if nothing
/// is left.
fn strip_trailing_thinking(messages: &mut Vec<Value>) {
    let Some(last) = messages.last() else {
        return;
    };
    if !is_assistant(last) {
        return;
    }
    let Some(Value::Array(parts)) = last.get("content") else {
        return;
    };
    let end = parts
        .iter()
        .rposition(|part| {
            !matches!(
                str_of(part.get("type")).trim(),
                "thinking" | "redacted_thinking"
            )
        })
        .map_or(0, |index| index + 1);
    if end == parts.len() {
        return;
    }
    if end == 0 {
        messages.pop();
        return;
    }
    let content = message_content(parts[..end].to_vec());
    if let Some(Value::Object(last)) = messages.last_mut() {
        last.insert("content".into(), content);
    }
}

/// `repairClaudeToolPairing`: Claude needs every `tool_use` answered by a
/// `tool_result` at the start of the very next user turn, and every
/// `tool_result` to answer a `tool_use` in the turn just before. Histories
/// break this when a session dies while a tool runs, when context lands
/// before a tool's output, or when an output arrives after a later assistant
/// turn. Missing results are made up as errors, and results with no call to
/// answer become text.
fn repair_tool_pairing(mut messages: Vec<Value>) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len() + 1);
    let mut previous_calls = HashSet::new();
    for i in 0..messages.len() {
        let mut message = std::mem::take(&mut messages[i]);
        let role = str_of(message.get("role")).into_owned();
        if role == "user"
            && let Some(rebuilt) = normalize_tool_result_message(&message, &previous_calls)
        {
            message = rebuilt;
        }
        let calls: Vec<String> = if role == "assistant" {
            blocks_of_type(&message, "tool_use")
                .map(|block| str_of(block.get("id")).into_owned())
                .filter(|id| !id.is_empty())
                .collect()
        } else {
            Vec::new()
        };
        previous_calls = calls.iter().cloned().collect();
        out.push(message);
        if calls.is_empty() {
            continue;
        }

        let next = messages
            .get_mut(i + 1)
            .filter(|next| str_of(next.get("role")) == "user");
        let answered: HashSet<String> = next.as_deref().map_or_else(HashSet::new, |next| {
            blocks_of_type(next, "tool_result")
                .map(|block| str_of(block.get("tool_use_id")).into_owned())
                .collect()
        });
        let mut parts: Vec<Value> = calls
            .iter()
            .filter(|id| !answered.contains(*id))
            .map(|id| {
                json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "is_error": true,
                    "content": INTERRUPTED_TOOL_RESULT,
                })
            })
            .collect();
        if parts.is_empty() {
            continue;
        }
        match next {
            // Results lead the user turn that follows.
            Some(next) => {
                match next.get("content") {
                    Some(Value::Array(blocks)) => parts.extend(blocks.iter().cloned()),
                    Some(Value::String(text)) => parts.push(text_block(text.as_str())),
                    _ => {}
                }
                *next = json!({"role": "user", "content": parts});
            }
            None => out.push(json!({"role": "user", "content": parts})),
        }
    }
    out
}

/// `normalizeClaudeToolResultMessage`: rebuilds a user turn so its
/// `tool_result` blocks come first, and those that don't answer one of
/// `calls` become text. `None` if it needs neither.
fn normalize_tool_result_message(message: &Value, calls: &HashSet<String>) -> Option<Value> {
    let Some(Value::Array(content)) = message.get("content") else {
        return None;
    };
    let mut results = Vec::new();
    let mut others = Vec::new();
    let mut seen_other = false;
    let mut changed = false;
    for block in content {
        if str_of(block.get("type")) != "tool_result" {
            seen_other = true;
            others.push(block.clone());
            continue;
        }
        if calls.contains(&*str_of(block.get("tool_use_id"))) {
            results.push(block.clone());
            changed |= seen_other;
            continue;
        }
        changed = true;
        seen_other = true;
        let text = tool_result_text_parts(block);
        if text.is_empty() {
            // Folded to nothing, it could leave an empty user turn.
            others.push(text_block(EMPTY_TOOL_RESULT));
        } else {
            others.extend(text);
        }
    }
    if !changed {
        return None;
    }
    results.extend(others);
    Some(json!({"role": "user", "content": results}))
}

/// `toolResultTextParts`: a `tool_result` with no call to answer, as plain
/// content blocks. Blank text is left out, as Claude rejects it.
fn tool_result_text_parts(block: &Value) -> Vec<Value> {
    match block.get("content") {
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| {
                let mut part = part.clone();
                if str_of(part.get("type")).is_empty()
                    && let Value::Object(fields) = &mut part
                {
                    fields.insert("type".into(), "text".into());
                }
                part
            })
            .filter(has_visible_content)
            .collect(),
        content => {
            let text = str_of(content);
            if text.trim().is_empty() {
                return Vec::new();
            }
            vec![text_block(text)]
        }
    }
}

/// `claudeModelRejectsAssistantPrefill`: whether the model rejects a
/// conversation ending with an assistant turn: Fable, Opus 5 and later, and
/// Sonnet 4.6 and later. The family is read after any provider namespace
/// and `claude-` prefix, and the versions after it, with `.` read as `-`.
fn rejects_assistant_prefill(model_name: &str) -> bool {
    let normalized = go::to_lower(model_name.trim());
    // Provider namespaces are not part of the model family.
    let normalized = normalized.rsplit('/').next().unwrap_or_default();
    let normalized = normalized.strip_prefix("claude-").unwrap_or(normalized);
    let normalized = normalized.replace('.', "-");
    let tokens: Vec<&str> = normalized.split('-').collect();
    let family = tokens.first().copied().unwrap_or_default();
    if family == "fable" {
        return true;
    }
    if family != "opus" && family != "sonnet" {
        return false;
    }
    let version = |position: usize| {
        tokens
            .get(position)
            .and_then(|token| prefill_version(token))
    };
    match version(1) {
        Some(major) if major >= 5 => true,
        Some(4) => family == "sonnet" && version(2).is_some_and(|minor| minor >= 6),
        _ => false,
    }
}

/// A model name's version number: up to seven ASCII digits. Eight digits
/// make a snapshot date, which is never a version.
fn prefill_version(token: &str) -> Option<u32> {
    if token.is_empty() || token.len() >= 8 || !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    token.parse().ok()
}

/// `claudeMessageInvariantProblems`: the ways the messages still break
/// Claude's tool pairing rules. Upstream logs them; we only test for them.
#[cfg(test)]
pub(super) fn message_invariant_problems(messages: &[Value]) -> Vec<String> {
    let mut problems = Vec::new();
    if messages
        .first()
        .is_some_and(|first| str_of(first.get("role")) != "user")
    {
        problems.push("first message is not user".to_owned());
    }
    for (i, message) in messages.iter().enumerate() {
        match &*str_of(message.get("role")) {
            "assistant" => {
                let calls: Vec<String> = blocks_of_type(message, "tool_use")
                    .map(|block| str_of(block.get("id")).into_owned())
                    .collect();
                if calls.is_empty() {
                    continue;
                }
                let Some(next) = messages
                    .get(i + 1)
                    .filter(|next| str_of(next.get("role")) == "user")
                else {
                    problems.push(format!(
                        "messages[{i}] tool_use has no following user message"
                    ));
                    continue;
                };
                let answered: HashSet<String> = blocks_of_type(next, "tool_result")
                    .map(|block| str_of(block.get("tool_use_id")).into_owned())
                    .collect();
                for id in calls.iter().filter(|id| !answered.contains(*id)) {
                    problems.push(format!(
                        "messages[{i}] tool_use {id} has no tool_result in messages[{}]",
                        i + 1
                    ));
                }
            }
            "user" => {
                let calls: HashSet<String> = match i.checked_sub(1) {
                    Some(previous) => blocks_of_type(&messages[previous], "tool_use")
                        .map(|block| str_of(block.get("id")).into_owned())
                        .collect(),
                    None => HashSet::new(),
                };
                let mut leading = true;
                for block in blocks(message) {
                    if str_of(block.get("type")) != "tool_result" {
                        leading = false;
                        continue;
                    }
                    if !leading {
                        problems.push(format!(
                            "messages[{i}] tool_result after non-tool_result block"
                        ));
                    }
                    let id = str_of(block.get("tool_use_id"));
                    if !calls.contains(&*id) {
                        problems.push(format!(
                            "messages[{i}] tool_result {id} has no tool_use in the previous message"
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    problems
}

/// `convertResponsesToolDescriptorToClaude`: a winning declaration as a
/// Claude tool named `claude_name`.
fn convert_tool(descriptor: &Descriptor, claude_name: &str) -> Option<Value> {
    let name = if claude_name.is_empty() && !descriptor.direct {
        &descriptor.name
    } else {
        claude_name
    };
    let tool = &descriptor.tool;
    match &*descriptor.kind {
        "function" => function_tool(tool, name),
        "custom" => custom_tool(tool, name),
        "web_search" => web_search_tool(tool),
        kind => {
            if is_unsupported_builtin_tool_type(kind) || str_of(tool.get("name")).is_empty() {
                return None;
            }
            Some(tool.clone())
        }
    }
}

/// `name`, or else the tool's own name made valid for Claude. `None` if both
/// are empty.
fn claude_tool_name(tool: &Value, name: &str) -> Option<String> {
    let name = match name.trim() {
        "" => sanitize_function_name(&tool_name(tool)),
        name => name.to_owned(),
    };
    (!name.is_empty()).then_some(name)
}

/// `convertResponsesFunctionToolToClaude`.
fn function_tool(tool: &Value, name: &str) -> Option<Value> {
    let mut out = Map::new();
    out.insert("name".into(), claude_tool_name(tool, name)?.into());
    out.insert("description".into(), tool_description(tool).into());
    out.insert(
        "input_schema".into(),
        normalize_claude_tool_input_schema(tool_parameters(tool)),
    );
    cache_control::attach(&mut out, tool);
    if !out.contains_key("cache_control")
        && let Some(function) = tool.get("function")
    {
        cache_control::attach(&mut out, function);
    }
    Some(Value::Object(out))
}

/// `convertResponsesCustomToolToClaude`: a freeform tool as a function of one
/// string, `input`. The `apply_patch` tool also gets its grammar explained.
fn custom_tool(tool: &Value, name: &str) -> Option<Value> {
    let mut out = Map::new();
    out.insert("name".into(), claude_tool_name(tool, name)?.into());
    out.insert("description".into(), tool_description(tool).into());
    out.insert(
        "input_schema".into(),
        json!({
            "type": "object",
            "properties": {"input": {"type": "string"}},
            "required": ["input"],
        }),
    );
    if apply_patch::is_custom_tool(tool) {
        out.insert("description".into(), apply_patch::description(tool).into());
        out.insert("input_schema".into(), apply_patch::parameters());
    }
    cache_control::attach(&mut out, tool);
    Some(Value::Object(out))
}

/// `convertResponsesWebSearchToolToClaude`: OpenAI's web search as Claude's.
fn web_search_tool(tool: &Value) -> Option<Value> {
    if !allows_external_web_access(tool) {
        return None;
    }
    let name = str_of(tool.get("name"));
    let name = match name.trim() {
        "" => "web_search",
        name => name,
    };
    let mut out = Map::new();
    out.insert("type".into(), "web_search_20250305".into());
    out.insert("name".into(), name.into());
    if let Some(max_uses) = tool.get("max_uses") {
        out.insert("max_uses".into(), int_of(max_uses).into());
    }
    if let Some(domains @ Value::Array(_)) = path(tool, "filters.allowed_domains") {
        out.insert("allowed_domains".into(), domains.clone());
    }
    if let Some(location @ Value::Object(_)) = tool.get("user_location") {
        out.insert("user_location".into(), location.clone());
    }
    Some(Value::Object(out))
}

/// `tool_choice`: `auto`, `required` if any tool survived, or one named
/// function or custom tool that did. Others are left out.
fn convert_tool_choice(
    choice: Option<&Value>,
    tools: &RequestTools,
    included: &HashSet<String>,
    name_map: &HashMap<String, String>,
) -> Option<Value> {
    match choice? {
        Value::String(choice) => match choice.as_str() {
            "auto" => Some(json!({"type": "auto"})),
            "required" if !included.is_empty() => Some(json!({"type": "any"})),
            _ => None,
        },
        choice @ Value::Object(_) => {
            let kind = str_of(choice.get("type"));
            if kind != "function" && kind != "custom" {
                return None;
            }
            let first = |keys: [&str; 3]| {
                keys.into_iter()
                    .map(|key| str_of(path(choice, key)))
                    .find(|value| !value.is_empty())
                    .unwrap_or_default()
            };
            let mut name = first(["function.name", "custom.name", "name"]).into_owned();
            let namespace = first(["namespace", "function.namespace", "custom.namespace"]);
            if !namespace.is_empty() {
                name = qualify(&namespace, &name);
            }
            if let Some(mapped) = name_map.get(&name).filter(|mapped| !mapped.is_empty()) {
                name = mapped.clone();
            }
            included.contains(&name).then(|| {
                object([
                    ("name", tools.claude_name(&name).into()),
                    ("type", "tool".into()),
                ])
            })
        }
        _ => None,
    }
}

fn text_block(text: impl Into<Value>) -> Value {
    object([("type", "text".into()), ("text", text.into())])
}

/// `unwrapCustomToolInput`: the freeform input in a custom tool call's
/// arguments, `{"input": "..."}`, read as gjson reads it, even from malformed
/// JSON; a value that isn't a string is kept as written. Arguments gjson
/// finds no `input` in, such as ones a stream cut off inside it, are scanned
/// for an `"input"` string and read up to its closing quote or their end;
/// without one they are returned as they are.
pub(super) fn unwrap_custom_tool_input(arguments: &str) -> String {
    let trimmed = arguments.trim();
    match lenient::get(trimmed, "input") {
        Some(Found::String(input)) => return input,
        Some(Found::Number(input) | Found::Literal(input) | Found::Json(input)) => {
            return input.to_owned();
        }
        None => {}
    }
    let content = trimmed
        .find("\"input\"")
        .map(|index| trimmed[index + "\"input\"".len()..].trim())
        .and_then(|rest| rest.strip_prefix(':'))
        .and_then(|rest| rest.trim().strip_prefix('"'));
    match content {
        Some(content) => unescape_until_quote(content.as_bytes()),
        None => arguments.to_owned(),
    }
}

/// Decodes a JSON string's body up to its closing quote, or to the end if it
/// has none, as upstream's hand-written loop does: an escape it doesn't know
/// keeps its backslash, and a trailing backslash stays.
fn unescape_until_quote(content: &[u8]) -> String {
    let mut out = Vec::with_capacity(content.len());
    let mut escaped = false;
    let mut i = 0;
    while i < content.len() {
        let c = content[i];
        if !escaped {
            match c {
                b'\\' => escaped = true,
                b'"' => break,
                _ => out.push(c),
            }
            i += 1;
            continue;
        }
        escaped = false;
        match c {
            b'"' | b'\\' | b'/' => out.push(c),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'u' => {
                if i + 4 < content.len()
                    && let Some(unit) = hex4(&content[i + 1..i + 5])
                {
                    // A surrogate followed by another `\u` escape is read as
                    // a pair, even when they don't make one.
                    if (0xD800..0xE000).contains(&unit)
                        && i + 10 < content.len()
                        && &content[i + 5..i + 7] == b"\\u"
                        && let Some(low) = hex4(&content[i + 7..i + 11])
                    {
                        push_char(&mut out, decode_surrogates(unit, low));
                        i += 11;
                        continue;
                    }
                    push_char(&mut out, char::from_u32(unit).unwrap_or('\u{FFFD}'));
                    i += 5;
                    continue;
                }
                out.extend_from_slice(b"\\u");
            }
            _ => out.extend_from_slice(&[b'\\', c]),
        }
        i += 1;
    }
    if escaped {
        out.push(b'\\');
    }
    // Bytes are only dropped or added whole characters at a time, so this
    // is still UTF-8.
    String::from_utf8_lossy(&out).into_owned()
}

/// Four hex digits, and nothing else, as a UTF-16 code unit.
fn hex4(digits: &[u8]) -> Option<u32> {
    digits.iter().try_fold(0, |unit, &digit| {
        Some(unit << 4 | char::from(digit).to_digit(16)?)
    })
}

/// Go's `utf16.DecodeRune`: the character a surrogate pair encodes, or U+FFFD
/// if it isn't one.
fn decode_surrogates(high: u32, low: u32) -> char {
    if (0xD800..0xDC00).contains(&high) && (0xDC00..0xE000).contains(&low) {
        char::from_u32(0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)).unwrap_or('\u{FFFD}')
    } else {
        '\u{FFFD}'
    }
}

fn push_char(out: &mut Vec<u8>, c: char) {
    out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
}

#[cfg(test)]
mod tests;
