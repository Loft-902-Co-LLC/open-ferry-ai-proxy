// Ported from CLIProxyAPI internal/translator/claude/gemini/claude_gemini_request.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini `generateContent` request → Claude Messages request.
//!
//! The system instruction becomes a user turn of its own; the contents
//! become alternating user and assistant turns, with function calls as
//! `tool_use` blocks and function responses as `tool_result` blocks. A
//! thinking level or budget becomes adaptive thinking with an effort for
//! models that take one, or a thinking budget for older models; which kind a
//! model takes comes from the [`ModelCatalog`].
//!
//! Deviations from upstream:
//! - `metadata.user_id` is only set to an ID the client sent, in
//!   `metadata.user_id` or `user`. Without one, upstream derives an ID from
//!   the conversation; we don't make up user IDs.
//! - A `topP` that isn't a finite number, such as `1e400` or the string
//!   `"NaN"`, is left out. Go writes it as `+Inf` or `NaN`, which isn't JSON.
//! - Where upstream copies the client's JSON text into a string, we write the
//!   same JSON compactly. This applies to a `tool_result` taken from a whole
//!   `response` or from a `response.result` that isn't a string; to text, a
//!   tool description or a stop sequence read from a value that isn't a
//!   string; and to a schema `type` that isn't a string, which is lowercased
//!   as text.
//! - Lowercasing a schema `type` follows sjson's rules for setting a path
//!   where the value on the way was replaced by text, except two. sjson pads
//!   an array with nulls up to a numeric key; we pad with at most 1,024, and
//!   past that put the key in a new object, or set nothing in an existing
//!   array, so a short key can't build a huge array. And sjson corrupts the
//!   JSON when it sets a numeric key inside text holding a `[`, where we
//!   build the array its rules describe.
//! - Negative zero in a tool schema is written as `0`. Go writes `-0`.

use std::collections::VecDeque;

use serde_json::{Map, Value};

use crate::common::claude::{MessageAccumulator, client_user_id, sanitize_function_name};
use crate::go;
use crate::json::{bool_of, float_of, go_marshaled, int_of, object, path, str_of};
use crate::models::ModelCatalog;
use crate::thinking::summary::apply_translated_to_claude;
use crate::thinking::{budget_to_level, claude_effort, has_level, level_to_budget};

const DEFAULT_MAX_TOKENS: i64 = 32000;
const DRAFT_07_SCHEMA: &str = "http://json-schema.org/draft-07/schema#";
/// The most nulls we pad an array with when a schema `type` is lowercased.
const MAX_ARRAY_PADDING: usize = 1024;

/// Converts a Gemini request body into a Claude Messages request body for
/// `model_name`. `stream` is whether the client asked to stream. `models` says
/// which thinking settings the model takes; pass [`ModelCatalog::embedded`]
/// unless you have your own.
pub fn convert_gemini_request_to_claude(
    model_name: &str,
    request: &Value,
    stream: bool,
    models: &ModelCatalog,
) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), model_name.into());
    out.insert("max_tokens".into(), DEFAULT_MAX_TOKENS.into());
    out.insert("messages".into(), Value::Array(Vec::new()));
    let metadata = match client_user_id(request) {
        Some(user_id) => object([("user_id", user_id.into())]),
        None => Value::Object(Map::new()),
    };
    out.insert("metadata".into(), metadata);
    if let Some(Value::String(tier)) = request.get("service_tier") {
        out.insert("service_tier".into(), tier.clone().into());
    }

    if let Some(config) = request.get("generationConfig") {
        apply_generation_config(&mut out, config, model_name, models);
    }

    out.insert("messages".into(), convert_messages(request).into());

    if let Some(Value::Array(tools)) = request.get("tools") {
        let tools: Vec<Value> = tools
            .iter()
            .filter_map(|tool| match tool.get("functionDeclarations") {
                Some(Value::Array(declarations)) => Some(declarations),
                _ => None,
            })
            .flatten()
            .map(convert_tool)
            .collect();
        if !tools.is_empty() {
            out.insert("tools".into(), tools.into());
        }
    }

    let function_calling = match request.get("tool_config") {
        Some(config) => config.get("function_calling_config"),
        None => path(request, "toolConfig.functionCallingConfig"),
    };
    if let Some(choice) = function_calling.and_then(tool_choice) {
        out.insert("tool_choice".into(), choice);
    }

    out.insert("stream".into(), stream.into());

    let mut out = Value::Object(out);
    apply_translated_to_claude(&mut out, request, "gemini", model_name, models);
    out
}

/// `IsGeminiThoughtPart`: a part marked as the model's hidden reasoning.
fn is_thought(part: &Value) -> bool {
    part.get("thought").is_some_and(bool_of)
}

/// `generationConfig`: the output limit, `topP`, stop sequences and thinking.
fn apply_generation_config(
    out: &mut Map<String, Value>,
    config: &Value,
    model_name: &str,
    models: &ModelCatalog,
) {
    if let Some(max_tokens) = config.get("maxOutputTokens") {
        out.insert("max_tokens".into(), int_of(max_tokens).into());
    }
    if let Some(top_p) = config.get("topP").and_then(float_of) {
        out.insert("top_p".into(), top_p);
    }
    if let Some(Value::Array(stops)) = config.get("stopSequences") {
        let stops: Vec<Value> = stops.iter().map(text_of).collect();
        if !stops.is_empty() {
            out.insert("stop_sequences".into(), stops.into());
        }
    }
    let Some(thinking @ Value::Object(_)) = config.get("thinkingConfig") else {
        return;
    };
    let levels = models
        .thinking(model_name)
        .map(|support| &support.levels)
        .filter(|levels| !levels.is_empty());
    let supports_max = levels.is_some_and(|levels| has_level(levels, "max"));
    let adaptive = levels.is_some();

    let setting = if let Some(level) = thinking
        .get("thinkingLevel")
        .or_else(|| thinking.get("thinking_level"))
    {
        let level = go::to_lower(str_of(Some(level)).trim());
        match level.as_str() {
            "" => None,
            "none" => Some(Thinking::Disabled),
            "auto" if !adaptive => Some(Thinking::Enabled(None)),
            _ if adaptive => {
                let effort = claude_effort(&level, supports_max).map_or(level, str::to_owned);
                Some(Thinking::Adaptive(effort))
            }
            _ => level_to_budget(&level).map(|budget| Thinking::Enabled(Some(budget))),
        }
    } else if let Some(budget) = thinking
        .get("thinkingBudget")
        .or_else(|| thinking.get("thinking_budget"))
    {
        match int_of(budget) {
            0 => Some(Thinking::Disabled),
            budget if adaptive => budget_to_level(budget).map(|level| {
                let effort = claude_effort(level, supports_max).unwrap_or(level);
                Thinking::Adaptive(effort.to_owned())
            }),
            -1 => Some(Thinking::Enabled(None)),
            budget => Some(Thinking::Enabled(Some(budget))),
        }
    } else {
        None
    };

    match setting {
        None => {}
        Some(Thinking::Disabled) => {
            out.insert("thinking".into(), object([("type", "disabled".into())]));
        }
        Some(Thinking::Enabled(budget)) => {
            let mut thinking = Map::new();
            thinking.insert("type".into(), "enabled".into());
            if let Some(budget) = budget {
                thinking.insert("budget_tokens".into(), budget.into());
            }
            out.insert("thinking".into(), thinking.into());
        }
        Some(Thinking::Adaptive(effort)) => {
            out.insert("thinking".into(), object([("type", "adaptive".into())]));
            out.insert("output_config".into(), object([("effort", effort.into())]));
        }
    }
}

/// The thinking setting a Gemini thinking config asks for.
enum Thinking {
    Disabled,
    /// Thinking with a budget, or with Claude's default.
    Enabled(Option<i64>),
    /// Adaptive thinking with an effort.
    Adaptive(String),
}

/// gjson `String()` as a JSON string.
fn text_of(value: &Value) -> Value {
    Value::String(str_of(Some(value)).into_owned())
}

fn text_block(text: impl Into<Value>) -> Value {
    object([("type", "text".into()), ("text", text.into())])
}

/// The system instruction, as a user turn of its own, then the contents as
/// alternating turns.
fn convert_messages(request: &Value) -> Vec<Value> {
    let mut messages = Vec::new();
    // Upstream reads only `system_instruction`, not `systemInstruction`.
    if let Some(Value::Array(parts)) = path(request, "system_instruction.parts") {
        // A newline goes before each text once there is some text, so empty
        // texts at the start add none.
        let mut text = String::new();
        for part_text in parts
            .iter()
            .filter(|part| !is_thought(part))
            .filter_map(|part| part.get("text"))
        {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&str_of(Some(part_text)));
        }
        if !text.is_empty() {
            messages.push(object([
                ("role", "user".into()),
                ("content", Value::Array(vec![text_block(text)])),
            ]));
        }
    }

    let Some(Value::Array(contents)) = request.get("contents") else {
        return messages;
    };
    let mut accumulator = MessageAccumulator::default();
    let mut tool_ids = ToolIds::default();
    for content in contents {
        let role = match str_of(content.get("role")).as_ref() {
            "model" | "assistant" => "assistant",
            "user" | "function" | "tool" => "user",
            // Upstream drops turns of any other role.
            _ => "",
        };
        let Some(Value::Array(parts)) = content.get("parts") else {
            continue;
        };
        let blocks: Vec<Value> = parts
            .iter()
            .filter(|part| !is_thought(part))
            .filter_map(|part| convert_part(part, role, &mut tool_ids))
            .collect();
        if !role.is_empty() {
            accumulator.push(role, blocks);
        }
    }
    messages.extend(accumulator.into_messages());
    messages
}

/// One content part as a Claude block, if it converts to one.
fn convert_part(part: &Value, role: &str, tool_ids: &mut ToolIds) -> Option<Value> {
    if let Some(text) = part.get("text") {
        return Some(text_block(text_of(text)));
    }
    if role == "assistant"
        && let Some(call) = part.get("functionCall")
    {
        let id = tool_ids.call(call);
        let name = match call.get("name") {
            Some(name) => sanitize_function_name(&str_of(Some(name))),
            None => String::new(),
        };
        let input = match call.get("args") {
            Some(args @ Value::Object(_)) => args.clone(),
            _ => Value::Object(Map::new()),
        };
        return Some(object([
            ("type", "tool_use".into()),
            ("id", id.into()),
            ("name", name.into()),
            ("input", input),
        ]));
    }
    if let Some(response) = part.get("functionResponse") {
        let id = tool_ids.response(response);
        let content = if let Some(result) = path(response, "response.result") {
            text_of(result)
        } else if let Some(response) = response.get("response") {
            response.to_string().into()
        } else {
            "".into()
        };
        return Some(object([
            ("type", "tool_result".into()),
            ("tool_use_id", id.into()),
            ("content", content),
        ]));
    }
    if let Some(inline) = part.get("inlineData").or_else(|| part.get("inline_data")) {
        return inline_data_block(inline);
    }
    if let Some(file) = part.get("fileData").or_else(|| part.get("file_data")) {
        return file_data_block(file);
    }
    None
}

/// Pairs function calls with their responses. A call or response keeps an ID
/// the client gave it; otherwise calls get `toolu_gemini_<n>`, and a response
/// takes the oldest call still waiting for one.
#[derive(Default)]
struct ToolIds {
    pending: VecDeque<String>,
    counter: u64,
}

impl ToolIds {
    fn generate(&mut self) -> String {
        self.counter += 1;
        format!("toolu_gemini_{:016}", self.counter)
    }

    fn call(&mut self, call: &Value) -> String {
        let id = client_tool_id(call).unwrap_or_else(|| self.generate());
        self.pending.push_back(id.clone());
        id
    }

    fn response(&mut self, response: &Value) -> String {
        if let Some(id) = client_tool_id(response) {
            if let Some(index) = self.pending.iter().position(|pending| *pending == id) {
                self.pending.remove(index);
            }
            return id;
        }
        self.pending.pop_front().unwrap_or_else(|| self.generate())
    }
}

/// `getGeminiToolID`: the trimmed `id`, or else the trimmed `call_id`.
fn client_tool_id(node: &Value) -> Option<String> {
    ["id", "call_id"].into_iter().find_map(|key| {
        let id = str_of(node.get(key));
        let id = id.trim();
        (!id.is_empty()).then(|| id.to_owned())
    })
}

/// Inline data with both a MIME type and data: an image, a document, or else
/// text naming the media type.
fn inline_data_block(inline: &Value) -> Option<Value> {
    let mut mime_type = str_of(inline.get("mimeType"));
    if mime_type.is_empty() {
        mime_type = str_of(inline.get("mime_type"));
    }
    let data = str_of(inline.get("data"));
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    let lower = go::to_lower(&mime_type);
    let kind = if lower.starts_with("image/") {
        "image"
    } else if lower.starts_with("application/") || lower.starts_with("text/") {
        "document"
    } else {
        return Some(text_block(format!(
            "Media content: inline data (Type: {mime_type})"
        )));
    };
    Some(object([
        ("type", kind.into()),
        (
            "source",
            object([
                ("type", "base64".into()),
                ("media_type", mime_type.into_owned().into()),
                ("data", data.into_owned().into()),
            ]),
        ),
    ]))
}

/// File data with a URI: an image, a document, or else text naming the file.
fn file_data_block(file: &Value) -> Option<Value> {
    let mut uri = str_of(file.get("fileUri"));
    if uri.is_empty() {
        uri = str_of(file.get("file_uri"));
    }
    if uri.is_empty() {
        return None;
    }
    let mut mime_type = str_of(file.get("mimeType"));
    if mime_type.is_empty() {
        mime_type = str_of(file.get("mime_type"));
    }
    let lower = go::to_lower(&mime_type);
    if lower.starts_with("image/") {
        return Some(object([
            ("type", "image".into()),
            (
                "source",
                object([("type", "url".into()), ("url", uri.into_owned().into())]),
            ),
        ]));
    }
    if lower.starts_with("application/") || lower.starts_with("text/") {
        let mut source = Map::new();
        source.insert("type".into(), "url".into());
        source.insert("url".into(), uri.into_owned().into());
        if !mime_type.is_empty() {
            source.insert("media_type".into(), mime_type.into_owned().into());
        }
        return Some(object([
            ("type", "document".into()),
            ("source", source.into()),
        ]));
    }
    let mut text = format!("File: {uri}");
    if !mime_type.is_empty() {
        text.push_str(&format!(" (Type: {mime_type})"));
    }
    Some(text_block(text))
}

/// A function declaration as a Claude tool. Upstream passes it through Go's
/// `json.Marshal`, which sorts every object's keys.
fn convert_tool(declaration: &Value) -> Value {
    let name = match declaration.get("name") {
        Some(name) => sanitize_function_name(&str_of(Some(name))),
        None => String::new(),
    };
    let description = declaration
        .get("description")
        .map_or(Value::from(""), text_of);
    let input_schema = match declaration
        .get("parameters")
        .or_else(|| declaration.get("parametersJsonSchema"))
    {
        Some(parameters) => normalize_schema(parameters),
        None => object([
            ("type", "object".into()),
            ("properties", Value::Object(Map::new())),
        ]),
    };
    let mut tool = object([
        ("name", name.into()),
        ("description", description),
        ("input_schema", input_schema),
    ]);
    lowercase_types(&mut tool);
    go_marshaled(&tool)
}

/// `normalizeClaudeToolSchema`: closes the schema with
/// `additionalProperties: false` and marks it as draft 7. sjson turns a schema
/// that is neither an object nor an array into an object, and can't set a key
/// in an array.
fn normalize_schema(parameters: &Value) -> Value {
    let mut schema = match parameters {
        Value::Array(_) => return parameters.clone(),
        Value::Object(schema) => schema.clone(),
        _ => Map::new(),
    };
    if parameters.get("additionalProperties") != Some(&Value::Bool(false)) {
        schema.insert("additionalProperties".into(), false.into());
    }
    if !matches!(parameters.get("$schema"), Some(Value::String(current)) if current == DRAFT_07_SCHEMA)
    {
        schema.insert("$schema".into(), DRAFT_07_SCHEMA.into());
    }
    Value::Object(schema)
}

/// `lowercaseClaudeToolSchemaTypes`: every value under a `type` key, at any
/// depth, becomes its gjson `String()` lowercased, unless it is already a
/// lowercase string. Upstream collects the paths first, outermost first, and
/// sets each with sjson, so a `type` holding an object or array becomes text
/// before the `type` keys inside it are set again through that text.
fn lowercase_types(tool: &mut Value) {
    let mut paths = Vec::new();
    collect_type_paths(tool, &mut Vec::new(), &mut paths);
    for path in paths {
        let current = get(tool, &path);
        let lower = go::to_lower(&str_of(current));
        if matches!(current, Some(Value::String(text)) if *text == lower) {
            continue;
        }
        set(tool, &path, Value::String(lower));
    }
}

/// The path of every `type` key under `value`, each before those inside it.
fn collect_type_paths(value: &Value, prefix: &mut Vec<String>, paths: &mut Vec<Vec<String>>) {
    match value {
        Value::Object(fields) => {
            for (key, child) in fields {
                prefix.push(key.clone());
                if key == "type" {
                    paths.push(prefix.clone());
                }
                collect_type_paths(child, prefix, paths);
                prefix.pop();
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                prefix.push(index.to_string());
                collect_type_paths(child, prefix, paths);
                prefix.pop();
            }
        }
        _ => {}
    }
}

/// gjson `Get` for a path of keys: a key names an object's field, or an
/// array's index if it is all digits.
fn get<'v>(value: &'v Value, path: &[String]) -> Option<&'v Value> {
    path.iter().try_fold(value, |value, key| match value {
        Value::Object(fields) => fields.get(key),
        Value::Array(items) => array_index(key).and_then(|index| items.get(index)),
        _ => None,
    })
}

/// A key that sjson and gjson read as an array index: one or more ASCII
/// digits, leading zeros allowed.
fn array_index(key: &str) -> Option<usize> {
    if key.is_empty() || !key.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Too large to be an index into anything.
    Some(key.parse().unwrap_or(usize::MAX))
}

/// sjson `Set` for a path of keys. An existing field or element is followed;
/// a missing one, or a value on the way that is neither an object nor an
/// array, is replaced by new structure. sjson can't set a non-numeric key in
/// an array, so then nothing changes.
fn set(value: &mut Value, path: &[String], new: Value) {
    let Some((key, rest)) = path.split_first() else {
        *value = new;
        return;
    };
    match value {
        Value::Object(fields) => match fields.get_mut(key) {
            Some(child) => set(child, rest, new),
            None => {
                fields.insert(key.clone(), build(rest, new));
            }
        },
        Value::Array(items) => {
            let Some(index) = array_index(key) else {
                return;
            };
            if let Some(child) = items.get_mut(index) {
                set(child, rest, new);
            } else if index - items.len() <= MAX_ARRAY_PADDING {
                items.resize(index, Value::Null);
                items.push(build(rest, new));
            }
        }
        _ => *value = build(path, new),
    }
}

/// The structure sjson builds for the rest of a path where nothing is: an
/// array for an index, padded with nulls before it, or an object for a key.
fn build(path: &[String], new: Value) -> Value {
    let Some((key, rest)) = path.split_first() else {
        return new;
    };
    match array_index(key).filter(|index| *index <= MAX_ARRAY_PADDING) {
        Some(index) => {
            let mut items = vec![Value::Null; index];
            items.push(build(rest, new));
            Value::Array(items)
        }
        None => object([(key.as_str(), build(rest, new))]),
    }
}

/// `setClaudeToolChoiceFromGeminiToolConfig`.
fn tool_choice(config: &Value) -> Option<Value> {
    let mode = config.get("mode")?;
    Some(match str_of(Some(mode)).as_ref() {
        "AUTO" => object([("type", "auto".into())]),
        "NONE" => object([("type", "none".into())]),
        "ANY" => {
            let names = config
                .get("allowedFunctionNames")
                .or_else(|| config.get("allowed_function_names"));
            match names {
                Some(Value::Array(names)) if names.len() == 1 => object([
                    ("type", "tool".into()),
                    (
                        "name",
                        sanitize_function_name(&str_of(names.first())).into(),
                    ),
                ]),
                _ => object([("type", "any".into())]),
            }
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
