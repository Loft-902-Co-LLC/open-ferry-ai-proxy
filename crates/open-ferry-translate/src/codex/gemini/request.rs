// Ported from CLIProxyAPI internal/translator/codex/gemini/codex_gemini_request.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini `generateContent` request → Codex (OpenAI Responses) request.
//!
//! Deviations from upstream:
//! - Where upstream copies the client's raw JSON into a string, we write compact
//!   re-serialized JSON. The values are the same JSON. This applies to
//!   `function_call.arguments`, to a `function_call_output.output` taken
//!   from a whole `response` or from a `response.result` that isn't a string,
//!   and to text and tool descriptions read from a value that isn't a string.
//! - Tool names cut to 64 bytes are cut at a UTF-8 character boundary.
//!   Upstream slices bytes and can split a character.
//! - Every `type` string in the tools is lowercased. Upstream finds them by
//!   gjson path, so it misses one under a key holding path syntax other than
//!   `.`, `*` and `?`, such as `#` or `|`.

use std::collections::{HashMap, VecDeque};

use serde_json::{Map, Value};

use super::super::unique_names::{UniqueNames, truncate_bytes};
use crate::go;
use crate::json::{bool_of, int_of, object, path, str_of};
use crate::thinking::budget_to_level;

/// The Responses API limit on function names.
const NAME_LIMIT: usize = 64;
const DEFAULT_REASONING_EFFORT: &str = "medium";

/// Converts a Gemini request body into a Codex Responses request body for
/// `model_name`. Codex requests always stream and are never stored.
pub fn convert_gemini_request_to_codex(model_name: &str, request: &Value) -> Value {
    let short_names = build_short_name_map(&declared_names(request));

    let mut input = Vec::new();
    input.extend(system_message(request));
    if let Some(Value::Array(contents)) = request.get("contents") {
        let mut call_ids = CallIds::default();
        for content in contents {
            push_content(&mut input, content, &short_names, &mut call_ids);
        }
    }

    let mut out = Map::new();
    out.insert("model".into(), model_name.into());
    out.insert("instructions".into(), "".into());
    out.insert("input".into(), input.into());
    if let Some(tier) = service_tier(request.get("service_tier")) {
        out.insert("service_tier".into(), tier.into());
    }

    if let Some(Value::Array(tools)) = request.get("tools") {
        out.insert("tool_choice".into(), "auto".into());
        let declarations = tools
            .iter()
            .filter_map(|tool| match tool.get("functionDeclarations") {
                Some(Value::Array(declarations)) => Some(declarations),
                _ => None,
            })
            .flatten();
        let items: Vec<Value> = declarations
            .map(|declaration| convert_tool(declaration, &short_names))
            .collect();
        out.insert("tools".into(), items.into());
    }

    out.insert("parallel_tool_calls".into(), true.into());
    if let Some(config) = path(request, "toolConfig.functionCallingConfig") {
        set_tool_choice(&mut out, config);
    }

    out.insert(
        "reasoning".into(),
        object([("effort", reasoning_effort(request).into())]),
    );
    out.insert("stream".into(), true.into());
    out.insert("store".into(), false.into());
    out.insert(
        "include".into(),
        Value::Array(vec!["reasoning.encrypted_content".into()]),
    );

    if let Some(tools) = out.get_mut("tools") {
        lowercase_types(tools);
    }
    Value::Object(out)
}

/// `IsGeminiThoughtPart`: a part marked as the model's hidden reasoning.
fn is_thought(part: &Value) -> bool {
    part.get("thought").is_some_and(bool_of)
}

/// The system instruction's text parts as one developer message. Upstream
/// reads `system_instruction.parts` if it's there at all, and only otherwise
/// `systemInstruction.parts`.
fn system_message(request: &Value) -> Option<Value> {
    let parts = path(request, "system_instruction.parts")
        .or_else(|| path(request, "systemInstruction.parts"));
    let Some(Value::Array(parts)) = parts else {
        return None;
    };
    let content: Vec<Value> = parts
        .iter()
        .filter(|part| !is_thought(part))
        .filter_map(|part| part.get("text"))
        .map(|text| object([("type", "input_text".into()), ("text", text_of(text))]))
        .collect();
    (!content.is_empty()).then(|| {
        object([
            ("type", "message".into()),
            ("role", "developer".into()),
            ("content", content.into()),
        ])
    })
}

/// One Gemini content turn. Each part becomes its own input item.
fn push_content(
    input: &mut Vec<Value>,
    content: &Value,
    short_names: &HashMap<String, String>,
    call_ids: &mut CallIds,
) {
    let mut role = str_of(content.get("role")).into_owned();
    if role == "model" {
        role = "assistant".to_owned();
    }
    let Some(Value::Array(parts)) = content.get("parts") else {
        return;
    };
    for part in parts {
        if is_thought(part) {
            continue;
        }
        if let Some(text) = part.get("text") {
            let kind = if role == "assistant" {
                "output_text"
            } else {
                "input_text"
            };
            let part = object([("type", kind.into()), ("text", text_of(text))]);
            input.push(message_with_part(&role, part));
            continue;
        }
        if let Some(part) = inline_data_part(part).or_else(|| file_data_part(part)) {
            input.push(message_with_part(&role, part));
            continue;
        }
        if let Some(call) = part.get("functionCall") {
            let mut item = Map::new();
            item.insert("type".into(), "function_call".into());
            if let Some(name) = call.get("name") {
                item.insert(
                    "name".into(),
                    codex_name(short_names, &str_of(Some(name))).into(),
                );
            }
            if let Some(args) = call.get("args") {
                item.insert("arguments".into(), args.to_string().into());
            }
            item.insert("call_id".into(), call_ids.call(call).into());
            input.push(Value::Object(item));
            continue;
        }
        if let Some(response) = part.get("functionResponse") {
            let mut item = Map::new();
            item.insert("type".into(), "function_call_output".into());
            if let Some(result) = path(response, "response.result") {
                item.insert("output".into(), text_of(result));
            } else if let Some(response) = response.get("response") {
                item.insert("output".into(), response.to_string().into());
            }
            item.insert("call_id".into(), call_ids.response(response).into());
            input.push(Value::Object(item));
        }
    }
}

/// gjson `String()` as a JSON string.
fn text_of(value: &Value) -> Value {
    Value::String(str_of(Some(value)).into_owned())
}

fn message_with_part(role: &str, part: Value) -> Value {
    object([
        ("type", "message".into()),
        ("role", role.into()),
        ("content", Value::Array(vec![part])),
    ])
}

/// Pairs function calls with their responses. A call or response keeps an ID
/// the client gave it; otherwise calls get `call_gemini_<n>`, and a response
/// takes the oldest call still waiting for one.
#[derive(Default)]
struct CallIds {
    pending: VecDeque<String>,
    counter: u64,
}

impl CallIds {
    fn generate(&mut self) -> String {
        self.counter += 1;
        format!("call_gemini_{:016}", self.counter)
    }

    fn call(&mut self, call: &Value) -> String {
        let id = client_call_id(call).unwrap_or_else(|| self.generate());
        self.pending.push_back(id.clone());
        id
    }

    fn response(&mut self, response: &Value) -> String {
        if let Some(id) = client_call_id(response) {
            if let Some(index) = self.pending.iter().position(|pending| *pending == id) {
                self.pending.remove(index);
            }
            return id;
        }
        self.pending.pop_front().unwrap_or_else(|| self.generate())
    }
}

/// `getGeminiCallID`: the trimmed `id`, or else the trimmed `call_id`.
fn client_call_id(node: &Value) -> Option<String> {
    ["id", "call_id"].into_iter().find_map(|key| {
        let id = str_of(node.get(key));
        let id = id.trim();
        (!id.is_empty()).then(|| id.to_owned())
    })
}

/// `inlineData` (or `inline_data`) with both a MIME type and data, as an
/// image, audio or file input.
fn inline_data_part(part: &Value) -> Option<Value> {
    let inline = part.get("inlineData").or_else(|| part.get("inline_data"))?;
    let mut mime_type = str_of(inline.get("mimeType"));
    if mime_type.is_empty() {
        mime_type = str_of(inline.get("mime_type"));
    }
    let data = str_of(inline.get("data"));
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    let lower = go::to_lower(&mime_type);
    Some(if lower.starts_with("image/") {
        object([
            ("type", "input_image".into()),
            (
                "image_url",
                format!("data:{mime_type};base64,{data}").into(),
            ),
        ])
    } else if lower.starts_with("audio/") {
        object([
            ("type", "input_audio".into()),
            (
                "input_audio",
                object([
                    ("data", data.into_owned().into()),
                    ("format", audio_format(&mime_type).into()),
                ]),
            ),
        ])
    } else {
        object([
            ("type", "input_file".into()),
            ("file_data", data.into_owned().into()),
            ("filename", file_name(&mime_type).into()),
        ])
    })
}

/// `fileData` (or `file_data`) with a URI, as an image or file input, or else
/// as text naming the file.
fn file_data_part(part: &Value) -> Option<Value> {
    let file = part.get("fileData").or_else(|| part.get("file_data"))?;
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
            ("type", "input_image".into()),
            ("image_url", uri.into_owned().into()),
        ]));
    }
    if ["video/", "application/", "text/"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return Some(object([
            ("type", "input_file".into()),
            ("file_url", uri.into_owned().into()),
            ("filename", file_name(&mime_type).into()),
        ]));
    }
    let mut text = format!("File: {uri}");
    if !mime_type.is_empty() {
        text.push_str(&format!(" (Type: {mime_type})"));
    }
    Some(object([
        ("type", "input_text".into()),
        ("text", text.into()),
    ]))
}

fn audio_format(mime_type: &str) -> &'static str {
    match go::to_lower(mime_type.trim()).as_str() {
        "audio/wav" | "audio/wave" | "audio/x-wav" => "wav",
        "audio/flac" => "flac",
        "audio/opus" | "audio/ogg" => "opus",
        "audio/pcm" | "audio/l16" => "pcm16",
        _ => "mp3",
    }
}

fn file_name(mime_type: &str) -> &'static str {
    let lower = go::to_lower(mime_type.trim());
    match lower.as_str() {
        "application/pdf" => "document.pdf",
        "text/plain" => "document.txt",
        "text/csv" => "document.csv",
        "application/json" => "document.json",
        "application/xml" | "text/xml" => "document.xml",
        _ if lower.starts_with("video/") => "video",
        _ => "document",
    }
}

/// `service_tier`, if it's a string naming the priority tier.
fn service_tier(tier: Option<&Value>) -> Option<&'static str> {
    let Some(Value::String(tier)) = tier else {
        return None;
    };
    matches!(go::to_lower(tier.trim()).as_str(), "priority" | "fast").then_some("priority")
}

fn convert_tool(declaration: &Value, short_names: &HashMap<String, String>) -> Value {
    let mut tool = Map::new();
    tool.insert("type".into(), "function".into());
    if let Some(name) = declaration.get("name") {
        tool.insert(
            "name".into(),
            codex_name(short_names, &str_of(Some(name))).into(),
        );
    }
    if let Some(description) = declaration.get("description") {
        tool.insert("description".into(), text_of(description));
    }
    if let Some(parameters) = declaration
        .get("parameters")
        .or_else(|| declaration.get("parametersJsonSchema"))
    {
        tool.insert("parameters".into(), clean_parameters(parameters));
    }
    tool.insert("strict".into(), false.into());
    Value::Object(tool)
}

/// `cleanGeminiCodexToolParameters`: drops `$schema` and closes the schema
/// with `additionalProperties: false`. sjson turns a schema that is neither an
/// object nor an array into an object, and can't set a key in an array.
fn clean_parameters(parameters: &Value) -> Value {
    match parameters {
        Value::Array(_) => parameters.clone(),
        Value::Object(schema) => {
            let mut schema = schema.clone();
            schema.shift_remove("$schema");
            if schema.get("additionalProperties") != Some(&Value::Bool(false)) {
                schema.insert("additionalProperties".into(), false.into());
            }
            Value::Object(schema)
        }
        _ => object([("additionalProperties", false.into())]),
    }
}

/// `setCodexToolChoiceFromGeminiToolConfig`. Only `ANY` with exactly one
/// allowed name picks a function, by its shortened name.
fn set_tool_choice(out: &mut Map<String, Value>, config: &Value) {
    let choice = match str_of(config.get("mode")).as_ref() {
        "NONE" => Value::from("none"),
        "AUTO" => Value::from("auto"),
        "ANY" => match config.get("allowedFunctionNames") {
            Some(Value::Array(names)) if names.len() == 1 => object([
                ("type", "function".into()),
                ("name", shorten_name(&str_of(names.first())).into()),
            ]),
            _ => Value::from("required"),
        },
        _ => return,
    };
    out.insert("tool_choice".into(), choice);
}

/// Codex's `reasoning.effort`: a thinking level, from `generationConfig` or
/// its `thinkingConfig`, or a thinking budget mapped to a level. Without
/// either, `medium`.
fn reasoning_effort(request: &Value) -> String {
    let Some(config) = request.get("generationConfig") else {
        return DEFAULT_REASONING_EFFORT.to_owned();
    };
    let effort = if let Some(level) = level(config) {
        level
    } else if let Some(thinking @ Value::Object(_)) = config.get("thinkingConfig") {
        if let Some(level) = level(thinking) {
            level
        } else {
            thinking
                .get("thinkingBudget")
                .or_else(|| thinking.get("thinking_budget"))
                .and_then(|budget| budget_to_level(int_of(budget)))
                .unwrap_or_default()
                .to_owned()
        }
    } else {
        String::new()
    };
    if effort.is_empty() {
        DEFAULT_REASONING_EFFORT.to_owned()
    } else {
        effort
    }
}

/// `thinkingLevel` (or `thinking_level`), trimmed and lowercased, if present.
fn level(config: &Value) -> Option<String> {
    let level = config
        .get("thinkingLevel")
        .or_else(|| config.get("thinking_level"))?;
    Some(go::to_lower(str_of(Some(level)).trim()))
}

/// Lowercases every string under a `type` key, at any depth.
fn lowercase_types(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for (key, child) in fields.iter_mut() {
                if key == "type"
                    && let Value::String(text) = child
                {
                    let lower = go::to_lower(text);
                    if lower != *text {
                        *text = lower;
                    }
                }
                lowercase_types(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(lowercase_types),
        _ => {}
    }
}

/// The names of every function declaration that has one, in order.
pub(super) fn declared_names(request: &Value) -> Vec<String> {
    let Some(Value::Array(tools)) = request.get("tools") else {
        return Vec::new();
    };
    tools
        .iter()
        .filter_map(|tool| match tool.get("functionDeclarations") {
            Some(Value::Array(declarations)) => Some(declarations),
            _ => None,
        })
        .flatten()
        .filter_map(|declaration| declaration.get("name"))
        .map(|name| str_of(Some(name)).into_owned())
        .collect()
}

/// `buildShortNameMap`: each declared name → a unique name within the limit.
/// When two declarations share a name, it maps to the later one's suffixed
/// name, as upstream's does.
pub(super) fn build_short_name_map(names: &[String]) -> HashMap<String, String> {
    let mut unique = UniqueNames::default();
    let mut map = HashMap::new();
    for name in names {
        map.insert(name.clone(), unique.claim(&shorten_name(name), NAME_LIMIT));
    }
    map
}

fn codex_name(short_names: &HashMap<String, String>, name: &str) -> String {
    match short_names.get(name) {
        Some(short) => short.clone(),
        None => shorten_name(name),
    }
}

/// `shortenNameIfNeeded`: fits a name within the limit. Long MCP names
/// (`mcp__server__tool`) keep the `mcp__` prefix and the tool part.
fn shorten_name(name: &str) -> String {
    if name.len() <= NAME_LIMIT {
        return name.to_owned();
    }
    if name.starts_with("mcp__")
        && let Some(index) = name.rfind("__")
    {
        let candidate = format!("mcp__{}", &name[index + 2..]);
        return truncate_bytes(&candidate, NAME_LIMIT).to_owned();
    }
    truncate_bytes(name, NAME_LIMIT).to_owned()
}

#[cfg(test)]
mod tests;
