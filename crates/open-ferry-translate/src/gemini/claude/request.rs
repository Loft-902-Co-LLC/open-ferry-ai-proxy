// Ported from CLIProxyAPI internal/translator/gemini/claude/gemini_claude_request.go
// (ConvertClaudeRequestToGemini, ConvertClaudeRequestToGeminiWithCompat,
// geminiContentWithParts and toolNameFromClaudeToolUseID) and
// internal/util/claude_tool_result.go (ConvertClaudeToolResultContent), with the
// document blocks and user turn checks (claudeBase64InlineData) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Claude Messages request → Gemini request.
//!
//! Messages become `contents`: `assistant` turns become `model` turns, and
//! `tool_use` and `tool_result` blocks become function calls and responses.
//! Mid-conversation `system` and `developer` messages become user turns
//! holding a `<system-reminder>`, and consecutive user turns are merged
//! ([`merge_adjacent_gemini_contents`]). Function names are made valid for
//! Gemini ([`sanitize_gemini_function_name`]); the response translator maps
//! them back. Tool schemas go through
//! [`clean_json_schema_for_gemini_json_schema`].
//!
//! Thinking blocks are dropped, except by
//! [`convert_claude_request_to_gemini_with_compat`]. Adaptive thinking without
//! an effort asks for the model's largest thinking budget, from the static
//! catalog ([`ModelCatalog`]).
//!
//! An image, document or container upload is sent only as base64 inline
//! data. A user's block that can't be (a URL or file source, say) is
//! dropped. The rest of the message is still sent, but a user message left
//! with nothing to send (blank text doesn't count) is refused with an
//! [`UnsupportedPartError`]. A message left with no parts at all isn't
//! sent.
//!
//! Deviations from upstream:
//! - Where upstream copies the client's JSON text, we write the same JSON
//!   compactly: a non-string `text` or name read as text, a tool call's
//!   `args` given as a string of JSON, and a tool result holding a `$ref`,
//!   which is stored as a string.
//! - A `temperature`, `top_p` or `top_k` that isn't a finite number, such as
//!   `1e400`, is left out. Go writes it as `+Inf`, which isn't JSON.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::common::claude::{
    align_tool_results, is_attribution_system_text, message_system_reminder_text,
};
use crate::common::gemini::{
    merge_adjacent_gemini_contents, reorder_gemini_user_parts, sanitize_gemini_function_name,
    set_gemini_function_response_result,
};
use crate::common::parts::{UserTurnDrops, count_sendable_gemini_parts};
use crate::gemini::common::attach_default_safety_settings;
use crate::gemini_schema::clean_json_schema_for_gemini_json_schema;
use crate::go;
use crate::json::{float_of, int_of, object, path, set_path, str_of};
use crate::models::ModelCatalog;
use crate::registry::UnsupportedPartError;
use crate::signature::{
    BlockKind, GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR, gemini_replay_signature_or_bypass,
};

/// The tool fields Gemini's function declarations don't take.
const DROPPED_TOOL_FIELDS: [&str; 6] = [
    "strict",
    "input_examples",
    "type",
    "cache_control",
    "defer_loading",
    "eager_input_streaming",
];

/// Converts a Claude Messages request body into a Gemini request body for
/// `model_name`. Assistant thinking blocks are dropped. The stream flag isn't
/// used: Gemini picks streaming by URL.
///
/// The error is set when a user message had only parts Gemini can't take;
/// the body is still returned.
pub fn convert_claude_request_to_gemini(
    model_name: &str,
    request: &Value,
    _stream: bool,
    models: &ModelCatalog,
) -> (Value, Option<UnsupportedPartError>) {
    convert(model_name, request, false, models)
}

/// [`convert_claude_request_to_gemini`] for compatibility endpoints, which
/// also get back assistant thinking blocks, as thought parts. A signature
/// Gemini didn't make is replaced by the validator bypass.
pub fn convert_claude_request_to_gemini_with_compat(
    model_name: &str,
    request: &Value,
    _stream: bool,
    models: &ModelCatalog,
) -> (Value, Option<UnsupportedPartError>) {
    convert(model_name, request, true, models)
}

fn convert(
    model_name: &str,
    request: &Value,
    keep_thinking: bool,
    models: &ModelCatalog,
) -> (Value, Option<UnsupportedPartError>) {
    let mut out = json!({"contents": [], "model": model_name});
    let mut drops = UserTurnDrops::default();

    if let Some(instruction) = system_instruction(request.get("system")) {
        out["systemInstruction"] = instruction;
    }
    if let Some(Value::Array(messages)) = request.get("messages") {
        out["contents"] = Value::Array(convert_messages(messages, keep_thinking, &mut drops));
    }

    let (tools, has_strict_tool) = convert_tools(request.get("tools"));
    let has_tools = !tools.is_empty();
    if has_tools {
        out["tools"] = json!([{ "functionDeclarations": tools }]);
    }
    apply_tool_choice(
        &mut out,
        request.get("tool_choice"),
        has_strict_tool,
        has_tools,
    );
    apply_thinking(&mut out, request, model_name, models);
    for (from, to) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("top_k", "topK"),
    ] {
        if let Some(value @ Value::Number(_)) = request.get(from)
            && let Some(value) = float_of(value)
        {
            set_path(&mut out, &format!("generationConfig.{to}"), value);
        }
    }

    attach_default_safety_settings(&mut out, "safetySettings");
    (out, drops.err())
}

/// The top-level system prompt, without Claude Code's attribution text. A
/// list of blocks gives a user-role instruction with one part per text
/// block; a string gives an instruction without a role.
fn system_instruction(system: Option<&Value>) -> Option<Value> {
    match system {
        Some(Value::Array(blocks)) => {
            let parts: Vec<Value> = blocks
                .iter()
                .filter(|block| str_of(block.get("type")) == "text")
                .filter_map(|block| match block.get("text") {
                    Some(Value::String(text)) if !is_attribution_system_text(text) => {
                        Some(json!({ "text": text }))
                    }
                    _ => None,
                })
                .collect();
            (!parts.is_empty()).then(|| json!({"role": "user", "parts": parts}))
        }
        Some(Value::String(text)) if !is_attribution_system_text(text) => {
            Some(json!({"parts": [{ "text": text }]}))
        }
        _ => None,
    }
}

/// Converts the messages into Gemini turns. A user message's blocks that
/// can't be sent are recorded in `drops`.
fn convert_messages(
    messages: &[Value],
    keep_thinking: bool,
    drops: &mut UserTurnDrops,
) -> Vec<Value> {
    let mut turns = Vec::with_capacity(messages.len());
    let mut tool_names: HashMap<String, String> = HashMap::new();
    // The tool calls of the last assistant message, which the next user
    // message's results are aligned to. System messages in between don't
    // count.
    let mut pending_tool_use_ids: Vec<String> = Vec::new();
    for message in messages {
        let Some(Value::String(original_role)) = message.get("role") else {
            continue;
        };
        let original_role = original_role.as_str();
        let is_system = matches!(original_role, "system" | "developer");
        let preceding_tool_use_ids = if is_system {
            Vec::new()
        } else {
            std::mem::take(&mut pending_tool_use_ids)
        };
        let content = message.get("content");
        if is_system {
            if let Some(reminder) = message_system_reminder_text(content) {
                turns.push(content_with_parts(
                    "user",
                    vec![json!({ "text": reminder })],
                ));
            }
            continue;
        }
        let role = if original_role == "assistant" {
            "model"
        } else {
            original_role
        };
        match content {
            Some(Value::Array(blocks)) => {
                let blocks = if original_role == "user" {
                    align_tool_results(blocks, &preceding_tool_use_ids)
                } else {
                    blocks.as_slice().into()
                };
                let mut parts = Vec::with_capacity(blocks.len());
                for block in blocks.iter() {
                    convert_block(
                        block,
                        original_role,
                        keep_thinking,
                        &mut tool_names,
                        &mut pending_tool_use_ids,
                        &mut parts,
                        drops,
                    );
                }
                if role == "user" {
                    parts = reorder_gemini_user_parts(parts);
                }
                if original_role == "user" {
                    // Blank text is sent, but doesn't keep an emptied turn alive.
                    drops.end_turn(count_sendable_gemini_parts(&parts));
                }
                if !parts.is_empty() {
                    turns.push(content_with_parts(role, parts));
                }
            }
            Some(Value::String(text)) => {
                turns.push(content_with_parts(role, vec![json!({ "text": text })]));
            }
            _ => {}
        }
    }

    // Gemini rejects a request ending with function calls it hasn't answered.
    if let Some(last) = turns.last()
        && str_of(last.get("role")) == "model"
        && let Some(Value::Array(parts)) = last.get("parts")
        && parts.iter().any(|part| part.get("functionCall").is_some())
    {
        turns.pop();
    }
    merge_adjacent_gemini_contents(turns)
}

/// Converts one content block into parts, if Gemini has a counterpart for
/// it. A user's media block that can't be sent is recorded in `drops`.
fn convert_block(
    block: &Value,
    original_role: &str,
    keep_thinking: bool,
    tool_names: &mut HashMap<String, String>,
    pending_tool_use_ids: &mut Vec<String>,
    parts: &mut Vec<Value>,
    drops: &mut UserTurnDrops,
) {
    match str_of(block.get("type")).as_ref() {
        "text" => {
            let text = str_of(block.get("text"));
            if !text.is_empty() {
                parts.push(json!({ "text": text }));
            }
        }
        "thinking" if keep_thinking => {
            let signature = gemini_replay_signature_or_bypass(
                &str_of(block.get("signature")),
                BlockKind::GeminiModelPart,
            );
            parts.push(json!({
                "text": str_of(block.get("thinking")),
                "thought": true,
                "thoughtSignature": signature,
            }));
        }
        "tool_use" => {
            let name = str_of(block.get("name"));
            let id = str_of(block.get("id")).into_owned();
            if !id.is_empty() && !name.is_empty() {
                tool_names.insert(id.clone(), name.to_string());
            }
            let Some(args) = tool_use_args(block.get("input")) else {
                return;
            };
            let mut call = json!({
                "name": sanitize_gemini_function_name(&name),
                "args": args,
            });
            if !id.is_empty() {
                call["id"] = Value::String(id.clone());
            }
            parts.push(json!({
                "thoughtSignature": GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
                "functionCall": call,
            }));
            if original_role == "assistant" {
                pending_tool_use_ids.push(id);
            }
        }
        "tool_result" => {
            let id = str_of(block.get("tool_use_id"));
            if id.is_empty() {
                return;
            }
            let mut name = tool_names.get(id.as_ref()).cloned().unwrap_or_default();
            if name.is_empty() {
                name = tool_name_from_tool_use_id(&id).to_owned();
            }
            if name.is_empty() {
                name = id.to_string();
            }
            let result = convert_tool_result_content(block.get("content"));
            let mut part = json!({"functionResponse": {
                "name": sanitize_gemini_function_name(&name),
                "response": {"result": ""},
                "id": id,
            }});
            set_gemini_function_response_result(
                &mut part,
                "functionResponse.response.result",
                Some(result.result),
            );
            parts.push(part);
            parts.extend(result.images.into_iter().map(inline_data));
        }
        block_type @ ("image" | "document" | "container_upload") => {
            if let Some(part) = base64_inline_data(block.get("source")) {
                parts.push(part);
            } else if original_role == "user" {
                // A URL or file source can't be inlined. The message is
                // refused only if nothing else is left.
                drops.drop_part(block_type);
            }
        }
        _ => {}
    }
}

/// `claudeBase64InlineData`: a base64 source with a media type and data, as
/// a Gemini inline data part.
fn base64_inline_data(source: Option<&Value>) -> Option<Value> {
    let source = source?;
    if str_of(source.get("type")) != "base64" {
        return None;
    }
    let mime_type = str_of(source.get("media_type"));
    let data = str_of(source.get("data"));
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    Some(inline_data(Image {
        mime_type: mime_type.into_owned(),
        data: data.into_owned(),
    }))
}

/// A `tool_use` block's input as function call arguments: an object, or a
/// string holding one. Anything else drops the call.
fn tool_use_args(input: Option<&Value>) -> Option<Value> {
    match input {
        Some(input @ Value::Object(_)) => Some(input.clone()),
        Some(Value::String(text)) => match serde_json::from_str(text) {
            Ok(args @ Value::Object(_)) => Some(args),
            _ => None,
        },
        _ => None,
    }
}

/// `toolNameFromClaudeToolUseID`: the ID up to its last `-`, or `""` if it
/// has none. IDs such as `Read-3` carry the tool's name.
fn tool_name_from_tool_use_id(id: &str) -> &str {
    id.rsplit_once('-').map_or("", |(name, _)| name)
}

/// `geminiContentWithParts`.
fn content_with_parts(role: &str, parts: Vec<Value>) -> Value {
    object([("role", Value::from(role)), ("parts", Value::Array(parts))])
}

/// A base64 image, as a Gemini inline data part.
fn inline_data(image: Image) -> Value {
    json!({"inline_data": {"mime_type": image.mime_type, "data": image.data}})
}

/// A base64 image taken out of a tool result.
#[derive(Debug, PartialEq)]
struct Image {
    mime_type: String,
    data: String,
}

/// A tool result's content as a Gemini function result (upstream's
/// `ClaudeToolResult`).
#[derive(Debug, PartialEq)]
struct ToolResult {
    /// The function result: text as a string, or the content's JSON.
    result: Value,
    /// The base64 images in the content, which go into parts of their own.
    images: Vec<Image>,
}

/// `ConvertClaudeToolResultContent`. Text stays text; a list of blocks gives
/// its one block that isn't a base64 image, or all of them as a list, or
/// `""` if there are none; any other value is kept as it is. Base64 images
/// with data are taken out, and those without are dropped. Missing content
/// gives `""`.
fn convert_tool_result_content(content: Option<&Value>) -> ToolResult {
    let empty = || Value::String(String::new());
    match content {
        Some(text @ Value::String(_)) => ToolResult {
            result: text.clone(),
            images: Vec::new(),
        },
        Some(Value::Array(blocks)) => {
            let mut images = Vec::new();
            let mut other = Vec::new();
            for block in blocks {
                if is_base64_image(block) {
                    images.extend(image_from_block(block));
                } else {
                    other.push(block.clone());
                }
            }
            let result = match other.len() {
                0 => empty(),
                1 => other.pop().expect("one block"),
                _ => Value::Array(other),
            };
            ToolResult { result, images }
        }
        Some(block @ Value::Object(_)) if is_base64_image(block) => ToolResult {
            result: empty(),
            images: image_from_block(block).into_iter().collect(),
        },
        Some(value) => ToolResult {
            result: value.clone(),
            images: Vec::new(),
        },
        None => ToolResult {
            result: empty(),
            images: Vec::new(),
        },
    }
}

/// `isClaudeBase64Image`.
fn is_base64_image(block: &Value) -> bool {
    str_of(block.get("type")) == "image" && str_of(path(block, "source.type")) == "base64"
}

/// `claudeImageFromBlock`: the image, unless it has no data.
fn image_from_block(block: &Value) -> Option<Image> {
    let data = str_of(path(block, "source.data"));
    (!data.is_empty()).then(|| Image {
        mime_type: str_of(path(block, "source.media_type")).into_owned(),
        data: data.into_owned(),
    })
}

/// Converts the Claude tool choice into Gemini's function calling mode.
/// Without one, a strict tool asks for validated calls.
fn apply_tool_choice(
    out: &mut Value,
    tool_choice: Option<&Value>,
    has_strict_tool: bool,
    has_tools: bool,
) {
    const MODE: &str = "toolConfig.functionCallingConfig.mode";
    let (kind, name) = match tool_choice {
        None | Some(Value::Null) => {
            if has_strict_tool && has_tools {
                set_path(out, MODE, Value::from("VALIDATED"));
            }
            return;
        }
        Some(choice @ Value::Object(_)) => (str_of(choice.get("type")), str_of(choice.get("name"))),
        Some(Value::String(kind)) => (kind.as_str().into(), "".into()),
        Some(_) => ("".into(), "".into()),
    };
    let mode = match kind.as_ref() {
        "auto" if has_strict_tool => "VALIDATED",
        "auto" => "AUTO",
        "none" => "NONE",
        "any" | "tool" => "ANY",
        _ => return,
    };
    set_path(out, MODE, Value::from(mode));
    if kind == "tool" && !name.is_empty() {
        set_path(
            out,
            "toolConfig.functionCallingConfig.allowedFunctionNames",
            json!([sanitize_gemini_function_name(&name)]),
        );
    }
}

/// Converts the tools that have an input schema into Gemini function
/// declarations. Also reports whether any tool is strict, schema or not.
fn convert_tools(tools: Option<&Value>) -> (Vec<Value>, bool) {
    let Some(Value::Array(tools)) = tools else {
        return (Vec::new(), false);
    };
    let mut declarations = Vec::new();
    let mut has_strict_tool = false;
    for tool in tools {
        if tool.get("strict") == Some(&Value::Bool(true)) {
            has_strict_tool = true;
        }
        let Value::Object(fields) = tool else {
            continue;
        };
        let Some(schema @ Value::Object(_)) = fields.get("input_schema") else {
            continue;
        };
        let parameters = clean_json_schema_for_gemini_json_schema(schema);
        let mut declaration = fields.clone();
        declaration.shift_remove("input_schema");
        declaration.insert("parametersJsonSchema".to_owned(), parameters);
        for field in DROPPED_TOOL_FIELDS {
            declaration.shift_remove(field);
        }
        let name = fields.get("name");
        let original = str_of(name);
        let sanitized = sanitize_gemini_function_name(&original);
        if !matches!(name, Some(Value::String(_))) || sanitized != original {
            declaration.insert("name".to_owned(), Value::String(sanitized));
        }
        declarations.push(Value::Object(declaration));
    }
    (declarations, has_strict_tool)
}

/// Converts Claude's thinking settings into Gemini's thinking config: a
/// budget for enabled thinking; for adaptive thinking, the requested effort
/// as a level, or else the model's largest budget, or else level `high`.
fn apply_thinking(out: &mut Value, request: &Value, model_name: &str, models: &ModelCatalog) {
    let Some(thinking @ Value::Object(_)) = request.get("thinking") else {
        return;
    };
    match str_of(thinking.get("type")).as_ref() {
        "enabled" => {
            if let Some(budget @ Value::Number(_)) = thinking.get("budget_tokens") {
                set_path(
                    out,
                    "generationConfig.thinkingConfig.thinkingBudget",
                    Value::from(int_of(budget)),
                );
            }
        }
        "adaptive" | "auto" => {
            let effort = match path(request, "output_config.effort") {
                Some(Value::String(effort)) => go::to_lower(effort.trim()),
                _ => String::new(),
            };
            let (key, value) = if !effort.is_empty() {
                ("thinkingLevel", Value::from(effort))
            } else {
                let max_budget = models.thinking(model_name).map_or(0, |support| support.max);
                if max_budget > 0 {
                    ("thinkingBudget", Value::from(max_budget))
                } else {
                    ("thinkingLevel", Value::from("high"))
                }
            };
            set_path(
                out,
                &format!("generationConfig.thinkingConfig.{key}"),
                value,
            );
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests;
