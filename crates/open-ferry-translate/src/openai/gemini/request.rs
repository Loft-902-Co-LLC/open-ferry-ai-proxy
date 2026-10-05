// Ported from CLIProxyAPI internal/translator/openai/gemini/openai_gemini_request.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini `generateContent` request → OpenAI Chat Completions request.
//!
//! The system instruction becomes a system message, and each content a
//! message of its own: function calls become the message's `tool_calls`, and
//! each function response a `tool` message just before it. A call or
//! response without an ID gets one derived from where it is in the request
//! and what it holds, so the same request always gets the same IDs.
//!
//! Deviations from upstream:
//! - Where upstream copies the client's JSON text into a string, we write the
//!   same JSON compactly. This applies to a tool call's `arguments`, a tool
//!   message's `content`, and text, a tool description or a stop sequence
//!   read from a value that isn't a string. A call or response ID derived
//!   from that JSON is then derived from the compact JSON, so it differs from
//!   upstream's when the client's JSON isn't compact.
//! - A `temperature` or `topP` that isn't a finite number, such as `1e400` or
//!   the string `"NaN"`, is left out. Go writes it as `+Inf` or `NaN`, which
//!   isn't JSON.

use std::collections::HashMap;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::go;
use crate::json::{bool_of, float_of, int_of, object, path, str_of};
use crate::thinking::budget_to_level;

/// Converts a Gemini request body into a Chat Completions request body for
/// `model_name`. `stream` is whether the client asked to stream.
pub fn convert_gemini_request_to_openai(model_name: &str, request: &Value, stream: bool) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), model_name.into());
    out.insert("messages".into(), Value::Array(Vec::new()));

    if let Some(config) = request.get("generationConfig") {
        apply_generation_config(&mut out, config);
    }

    out.insert("stream".into(), stream.into());
    if let Some(Value::String(tier)) = request.get("service_tier") {
        out.insert("service_tier".into(), tier.clone().into());
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

    if let Some(choice) = path(request, "toolConfig.functionCallingConfig").and_then(tool_choice) {
        out.insert("tool_choice".into(), choice);
    }

    Value::Object(out)
}

/// `IsGeminiThoughtPart`: a part marked as the model's hidden reasoning.
fn is_thought(part: &Value) -> bool {
    part.get("thought").is_some_and(bool_of)
}

/// gjson `String()` as a JSON string.
fn text_of(value: &Value) -> Value {
    Value::String(str_of(Some(value)).into_owned())
}

/// `generationConfig`: sampling, limits, stop sequences, output modalities
/// and the reasoning effort.
fn apply_generation_config(out: &mut Map<String, Value>, config: &Value) {
    if let Some(temperature) = config.get("temperature").and_then(float_of) {
        out.insert("temperature".into(), temperature);
    }
    if let Some(max_tokens) = config.get("maxOutputTokens") {
        out.insert("max_tokens".into(), int_of(max_tokens).into());
    }
    if let Some(top_p) = config.get("topP").and_then(float_of) {
        out.insert("top_p".into(), top_p);
    }
    if let Some(top_k) = config.get("topK") {
        out.insert("top_k".into(), int_of(top_k).into());
    }
    if let Some(Value::Array(stops)) = config.get("stopSequences")
        && !stops.is_empty()
    {
        out.insert("stop".into(), stops.iter().map(text_of).collect());
    }
    if let Some(count) = config.get("candidateCount") {
        out.insert("n".into(), int_of(count).into());
    }
    if let Some(Value::Array(modalities)) = config.get("responseModalities") {
        let modalities: Vec<Value> = modalities
            .iter()
            .filter_map(|modality| {
                let modality = go::to_lower(str_of(Some(modality)).trim());
                matches!(modality.as_str(), "text" | "image" | "audio").then(|| modality.into())
            })
            .collect();
        if !modalities.is_empty() {
            out.insert("modalities".into(), modalities.into());
        }
    }
    if let Some(thinking @ Value::Object(_)) = config.get("thinkingConfig") {
        let effort = match thinking
            .get("thinkingLevel")
            .or_else(|| thinking.get("thinking_level"))
        {
            Some(level) => {
                let level = go::to_lower(str_of(Some(level)).trim());
                (!level.is_empty()).then_some(level)
            }
            None => thinking
                .get("thinkingBudget")
                .or_else(|| thinking.get("thinking_budget"))
                .and_then(|budget| budget_to_level(int_of(budget)))
                .map(str::to_owned),
        };
        if let Some(effort) = effort {
            out.insert("reasoning_effort".into(), effort.into());
        }
    }
}

/// The system instruction as a system message, then a message per content,
/// each with the tool messages for its function responses before it.
fn convert_messages(request: &Value) -> Vec<Value> {
    let mut messages = Vec::new();

    let system = request
        .get("systemInstruction")
        .or_else(|| request.get("system_instruction"));
    if let Some(Value::Array(parts)) = system.and_then(|system| system.get("parts")) {
        let mut items = Vec::new();
        for part in parts.iter().filter(|part| !is_thought(part)) {
            if let Some(text) = part.get("text") {
                items.push(text_part(text_of(text)));
            }
            items.extend(inline_data_part(part));
            items.extend(file_data_part(part));
        }
        if !items.is_empty() {
            messages.push(object([
                ("role", "system".into()),
                ("content", items.into()),
            ]));
        }
    }

    let Some(Value::Array(contents)) = request.get("contents") else {
        return messages;
    };
    let mut pending = PendingCalls::default();
    for (message_index, content) in contents.iter().enumerate() {
        let mut role = str_of(content.get("role")).into_owned();
        if role == "model" {
            role = "assistant".to_owned();
        }

        let mut text = String::new();
        let mut items = Vec::new();
        let mut only_text = true;
        let mut tool_calls = Vec::new();
        let mut dropped_thought = false;

        if let Some(Value::Array(parts)) = content.get("parts") {
            for (part_index, part) in parts.iter().enumerate() {
                if is_thought(part) {
                    dropped_thought = true;
                    continue;
                }
                if let Some(part_text) = part.get("text") {
                    let part_text = str_of(Some(part_text)).into_owned();
                    text.push_str(&part_text);
                    items.push(text_part(part_text.into()));
                }
                for item in [inline_data_part(part), file_data_part(part)]
                    .into_iter()
                    .flatten()
                {
                    only_text = false;
                    items.push(item);
                }
                if let Some(call) = part.get("functionCall") {
                    tool_calls.push(pending.call(call, message_index, part_index));
                }
                if let Some(response) = part.get("functionResponse") {
                    messages.push(pending.response(response, message_index, part_index));
                }
            }
        }

        if dropped_thought && items.is_empty() && tool_calls.is_empty() {
            continue;
        }
        let mut message = Map::new();
        message.insert("role".into(), role.into());
        let content = match items.is_empty() {
            true => Value::from(""),
            false if only_text => text.into(),
            false => items.into(),
        };
        message.insert("content".into(), content);
        if !tool_calls.is_empty() {
            message.insert("tool_calls".into(), tool_calls.into());
        }
        messages.push(message.into());
    }
    messages
}

fn text_part(text: Value) -> Value {
    object([("type", "text".into()), ("text", text)])
}

/// Tool call IDs waiting for their responses, by function name.
#[derive(Default)]
struct PendingCalls {
    by_name: HashMap<String, Vec<String>>,
}

impl PendingCalls {
    /// A function call as a tool call, keeping the client's ID or deriving
    /// one, which then waits for its response.
    fn call(&mut self, call: &Value, message_index: usize, part_index: usize) -> Value {
        let name = str_of(call.get("name")).into_owned();
        let arguments = call.get("args").map(Value::to_string).unwrap_or_default();
        let id = explicit_tool_id(call).unwrap_or_else(|| {
            derived_tool_id("call", message_index, part_index, &name, &arguments)
        });
        self.by_name
            .entry(name.clone())
            .or_default()
            .push(id.clone());
        let arguments = if arguments.is_empty() {
            "{}".to_owned()
        } else {
            arguments
        };
        object([
            ("id", id.into()),
            ("type", "function".into()),
            (
                "function",
                object([("name", name.into()), ("arguments", arguments.into())]),
            ),
        ])
    }

    /// A function response as a tool message. It keeps the client's ID, or
    /// takes the oldest call of the same name still waiting, or else gets an
    /// ID derived from it.
    fn response(&mut self, response: &Value, message_index: usize, part_index: usize) -> Value {
        let name = str_of(response.get("name")).into_owned();
        let content = response
            .get("response")
            .map(|body| body.get("content").unwrap_or(body).to_string())
            .unwrap_or_default();
        let queue = self.by_name.entry(name.clone()).or_default();
        let id = if let Some(id) = explicit_tool_id(response) {
            if let Some(index) = queue.iter().position(|pending| *pending == id) {
                queue.remove(index);
            }
            id
        } else if !queue.is_empty() {
            queue.remove(0)
        } else {
            derived_tool_id("response", message_index, part_index, &name, &content)
        };
        object([
            ("role", "tool".into()),
            ("tool_call_id", id.into()),
            ("content", content.into()),
        ])
    }
}

/// `deterministicToolCallID`: `call_` and the first 12 bytes, in hex, of a
/// SHA-256 of where the call or response is and what it holds.
fn derived_tool_id(
    kind: &str,
    message_index: usize,
    part_index: usize,
    name: &str,
    payload: &str,
) -> String {
    let digest = Sha256::digest(format!(
        "{kind}|{message_index}|{part_index}|{name}|{payload}"
    ));
    let mut id = String::from("call_");
    for byte in &digest[..12] {
        id.push_str(&format!("{byte:02x}"));
    }
    id
}

/// `explicitGeminiToolID`: the first of `id`, `call_id` and `callId` that
/// isn't blank once trimmed.
fn explicit_tool_id(node: &Value) -> Option<String> {
    ["id", "call_id", "callId"].into_iter().find_map(|key| {
        let id = str_of(node.get(key));
        let id = id.trim();
        (!id.is_empty()).then(|| id.to_owned())
    })
}

/// Inline data with data in it, as an image, audio, video or file part by
/// its MIME type.
fn inline_data_part(part: &Value) -> Option<Value> {
    let inline = part.get("inlineData").or_else(|| part.get("inline_data"))?;
    let mut mime_type = str_of(inline.get("mimeType"));
    if mime_type.is_empty() {
        mime_type = str_of(inline.get("mime_type"));
    }
    if mime_type.is_empty() {
        mime_type = "application/octet-stream".into();
    }
    let data = str_of(inline.get("data"));
    if data.is_empty() {
        return None;
    }
    let data_url = format!("data:{mime_type};base64,{data}");
    let lower = go::to_lower(&mime_type);
    Some(if lower.starts_with("image/") {
        object([
            ("type", "image_url".into()),
            ("image_url", object([("url", data_url.into())])),
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
    } else if lower.starts_with("video/") {
        object([
            ("type", "video_url".into()),
            ("video_url", object([("url", data_url.into())])),
        ])
    } else {
        object([
            ("type", "file".into()),
            (
                "file",
                object([
                    ("filename", file_name(&mime_type).into()),
                    ("file_data", data.into_owned().into()),
                ]),
            ),
        ])
    })
}

/// File data with a URI, as an image, video or file part by its MIME type,
/// or else text naming the file.
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
    let uri = uri.into_owned();
    Some(if lower.starts_with("image/") {
        object([
            ("type", "image_url".into()),
            ("image_url", object([("url", uri.into())])),
        ])
    } else if lower.starts_with("video/") {
        object([
            ("type", "video_url".into()),
            ("video_url", object([("url", uri.into())])),
        ])
    } else if lower.starts_with("application/") || lower.starts_with("text/") {
        object([
            ("type", "file".into()),
            (
                "file",
                object([
                    ("filename", file_name(&mime_type).into()),
                    ("file_url", uri.into()),
                ]),
            ),
        ])
    } else {
        let mut text = format!("File: {uri}");
        if !mime_type.is_empty() {
            text.push_str(&format!(" (Type: {mime_type})"));
        }
        text_part(text.into())
    })
}

/// `openAIInputAudioFormatFromMIME`.
fn audio_format(mime_type: &str) -> &'static str {
    match go::to_lower(mime_type.trim()).as_str() {
        "audio/wav" | "audio/wave" | "audio/x-wav" => "wav",
        "audio/flac" => "flac",
        "audio/opus" | "audio/ogg" => "opus",
        "audio/pcm" | "audio/l16" => "pcm16",
        _ => "mp3",
    }
}

/// `openAIFileNameFromMIME`.
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

/// A function declaration as a Chat Completions tool. The parameters are
/// copied as they are.
fn convert_tool(declaration: &Value) -> Value {
    let mut function = Map::new();
    function.insert("name".into(), text_of_or_empty(declaration.get("name")));
    function.insert(
        "description".into(),
        text_of_or_empty(declaration.get("description")),
    );
    if let Some(parameters) = declaration
        .get("parameters")
        .or_else(|| declaration.get("parametersJsonSchema"))
    {
        function.insert("parameters".into(), parameters.clone());
    }
    object([("type", "function".into()), ("function", function.into())])
}

fn text_of_or_empty(value: Option<&Value>) -> Value {
    Value::String(str_of(value).into_owned())
}

/// `toolConfig.functionCallingConfig` as a tool choice.
fn tool_choice(config: &Value) -> Option<Value> {
    Some(match str_of(config.get("mode")).as_ref() {
        "NONE" => "none".into(),
        "AUTO" => "auto".into(),
        "ANY" => match config.get("allowedFunctionNames") {
            Some(Value::Array(names)) if names.len() == 1 => object([
                ("type", "function".into()),
                ("function", object([("name", text_of(&names[0]))])),
            ]),
            _ => "required".into(),
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
