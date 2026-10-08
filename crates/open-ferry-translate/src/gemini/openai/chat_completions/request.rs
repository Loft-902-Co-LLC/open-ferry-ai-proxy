// Ported from CLIProxyAPI internal/translator/gemini/openai/chat-completions/gemini_openai_request.go
// (ConvertOpenAIRequestToGemini, geminiTextPart, geminiInlineDataPart,
// geminiContentNode, openAIToolCallGeminiThoughtSignature,
// openAIInputAudioMimeType, applyOpenAIResponseFormatToGemini and
// geminiDemotedSystemText) and internal/util/translator.go (RenameKey)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! OpenAI Chat Completions request → Gemini request.
//!
//! Leading `system` and `developer` messages become the system instruction;
//! later ones become user turns holding a `<system-reminder>`. Turns are not
//! merged. An assistant message's tool calls become one model turn of
//! function calls, followed by one user turn answering each from the `tool`
//! messages before the next assistant message; each answer is the tool
//! message's content as JSON text, stored as a string. A trailing model turn
//! is dropped.
//!
//! Function names are made valid for Gemini
//! ([`sanitize_gemini_function_name`]). Tool parameters become
//! `parametersJsonSchema`, cleaned by
//! [`clean_json_schema_for_gemini_json_schema`]. `tool_choice` maps to a
//! function calling mode, failing closed to `NONE` where it can't be honoured:
//! two tools whose names sanitize alike, an unknown choice, a named tool that
//! isn't declared, or `parallel_tool_calls: false`. A strict tool asks for
//! `VALIDATED` mode.
//!
//! In a user message, or a system message that became a user turn, an image
//! or video is inlined only from a base64 `data:` URL, and a file only from
//! data whose type can be told; audio needs data. Anything else there is
//! dropped. The rest of the turn is still sent, but a turn left with nothing
//! to send is refused with an [`UnsupportedPartError`].
//!
//! Deviations from upstream:
//! - A tool call whose `arguments` isn't JSON, such as `""` or missing,
//!   gets no `args`. Upstream copies the text as it is, which makes the
//!   request invalid JSON.
//! - Where upstream copies the client's JSON text, we write the same JSON
//!   compactly: a tool message's content, stored as the text of a function
//!   response's `result`, and a non-string value read as text, such as a
//!   tool name that is an object.
//! - A `temperature`, `top_p`, `top_k`, `max_tokens` or
//!   `max_completion_tokens` that isn't a finite number, such as `1e400`, is
//!   left out. Go writes it as `+Inf`, which isn't JSON.
//! - When a key is repeated, the last one counts; gjson reads the first.
//! - Upstream logs a warning for a file it can't read; we log nothing.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use crate::common::claude::system_reminder_text;
use crate::common::file_data::normalize_openai_file_data;
use crate::common::gemini::sanitize_gemini_function_name;
use crate::common::parts::{UserTurnDrops, count_sendable_gemini_parts};
use crate::gemini::common::attach_default_safety_settings;
use crate::gemini_schema::clean_json_schema_for_gemini_json_schema;
use crate::go;
use crate::json::{delete_path, float_of, int_of, object, path, set_path, str_of};
use crate::registry::UnsupportedPartError;
use crate::signature::{
    BlockKind, GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR, gemini_replay_signature_or_bypass,
};

/// Where a tool call's thought signature may be, in the order upstream looks.
const TOOL_CALL_SIGNATURE_PATHS: [&str; 4] = [
    "extra_content.google.thought_signature",
    "function.extra_content.google.thought_signature",
    "thoughtSignature",
    "thought_signature",
];

const MODE: &str = "toolConfig.functionCallingConfig.mode";
const ALLOWED_FUNCTION_NAMES: &str = "toolConfig.functionCallingConfig.allowedFunctionNames";

/// Converts an OpenAI Chat Completions request body into a Gemini request
/// body for `model_name`. The stream flag isn't used: Gemini picks streaming
/// by URL.
///
/// The error is set when a user turn had only parts Gemini can't take; the
/// body is still returned.
pub fn convert_openai_request_to_gemini(
    model_name: &str,
    request: &Value,
    _stream: bool,
) -> (Value, Option<UnsupportedPartError>) {
    let mut out = json!({"contents": [], "model": model_name});
    let mut drops = UserTurnDrops::default();

    if let Some(config) = request.get("generationConfig") {
        out["generationConfig"] = config.clone();
    }
    apply_generation_settings(&mut out, request);

    if let Some(Value::Array(messages)) = request.get("messages") {
        let (system_parts, mut contents) = convert_messages(messages, &mut drops);
        if !system_parts.is_empty() {
            out["systemInstruction"] = content_node("user", system_parts);
        }
        if contents
            .last()
            .is_some_and(|content| str_of(content.get("role")) == "model")
        {
            contents.pop();
        }
        out["contents"] = Value::Array(contents);
    }

    let allowed = AllowedTools::read(request.get("tool_choice"));
    let tools = convert_tools(&mut out, request.get("tools"), allowed.as_ref());
    apply_tool_config(&mut out, request, &tools, allowed.as_ref());

    attach_default_safety_settings(&mut out, "safetySettings");
    (out, drops.err())
}

/// The `generationConfig` settings read from the request's OpenAI fields.
fn apply_generation_settings(out: &mut Value, request: &Value) {
    if let Some(effort) = request.get("reasoning_effort") {
        let effort = go::to_lower(str_of(Some(effort)).trim());
        if effort == "auto" {
            set_path(
                out,
                "generationConfig.thinkingConfig.thinkingBudget",
                json!(-1),
            );
        } else if !effort.is_empty() {
            set_path(
                out,
                "generationConfig.thinkingConfig.thinkingLevel",
                Value::String(effort),
            );
        }
    }

    for (from, to) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("top_k", "topK"),
    ] {
        if let Some(number @ Value::Number(_)) = request.get(from)
            && let Some(value) = float_of(number)
        {
            set_path(out, &format!("generationConfig.{to}"), value);
        }
    }

    // max_completion_tokens counts only when max_tokens isn't a number.
    let max_tokens = match request.get("max_tokens") {
        Some(number @ Value::Number(_)) => Some(number),
        _ => request
            .get("max_completion_tokens")
            .filter(|v| v.is_number()),
    };
    if let Some(value) = max_tokens.and_then(float_of) {
        set_path(out, "generationConfig.maxOutputTokens", value);
    }

    if let Some(n @ Value::Number(_)) = request.get("n") {
        let n = int_of(n);
        if n > 1 {
            set_path(out, "generationConfig.candidateCount", Value::from(n));
        }
    }

    apply_response_format(out, request.get("response_format"));

    if let Some(Value::Array(modalities)) = request.get("modalities") {
        let modalities: Vec<Value> = modalities
            .iter()
            .filter_map(
                |modality| match go::to_lower(&str_of(Some(modality))).as_str() {
                    "text" => Some(Value::from("TEXT")),
                    "image" => Some(Value::from("IMAGE")),
                    _ => None,
                },
            )
            .collect();
        if !modalities.is_empty() {
            set_path(
                out,
                "generationConfig.responseModalities",
                Value::Array(modalities),
            );
        }
    }

    if let Some(config @ Value::Object(_)) = request.get("image_config") {
        for (from, to) in [("aspect_ratio", "aspectRatio"), ("image_size", "imageSize")] {
            if let Some(value @ Value::String(_)) = config.get(from) {
                set_path(
                    out,
                    &format!("generationConfig.imageConfig.{to}"),
                    value.clone(),
                );
            }
        }
    }
}

/// `applyOpenAIResponseFormatToGemini`: JSON output for `json_object`, and
/// for `json_schema` with the schema, which replaces any `responseSchema`.
fn apply_response_format(out: &mut Value, format: Option<&Value>) {
    let Some(format) = format else {
        return;
    };
    match go::to_lower(str_of(format.get("type")).trim()).as_str() {
        "json_object" => {
            set_path(
                out,
                "generationConfig.responseMimeType",
                Value::from("application/json"),
            );
        }
        "json_schema" => {
            set_path(
                out,
                "generationConfig.responseMimeType",
                Value::from("application/json"),
            );
            delete_path(out, "generationConfig.responseSchema");
            if let Some(schema) = path(format, "json_schema.schema") {
                set_path(out, "generationConfig.responseJsonSchema", schema.clone());
            }
        }
        _ => {}
    }
}

/// The system instruction's parts and the contents, from the messages. Each
/// user turn is recorded in `drops`.
fn convert_messages(messages: &[Value], drops: &mut UserTurnDrops) -> (Vec<Value>, Vec<Value>) {
    let mut system_parts = Vec::new();
    let mut contents = Vec::new();
    let mut in_conversation = false;

    for (i, message) in messages.iter().enumerate() {
        let role = str_of(message.get("role"));
        let content = message.get("content");
        let is_system = role == "system" || role == "developer";

        if is_system && messages.len() > 1 && !in_conversation {
            match content {
                Some(Value::String(text)) => system_parts.push(text_part(text.clone())),
                Some(block @ Value::Object(_)) if str_of(block.get("type")) == "text" => {
                    system_parts.push(text_part(str_of(block.get("text")).into_owned()));
                }
                Some(Value::Array(items)) => system_parts.extend(
                    items
                        .iter()
                        .map(|item| text_part(str_of(item.get("text")).into_owned())),
                ),
                _ => {}
            }
        } else if role == "user" || is_system {
            in_conversation = true;
            let parts = user_parts(content, is_system, drops);
            // Whitespace-only text is sent but doesn't keep an emptied turn.
            drops.end_turn(count_sendable_gemini_parts(&parts));
            if !parts.is_empty() {
                contents.push(content_node("user", parts));
            }
        } else if role == "assistant" {
            in_conversation = true;
            convert_assistant(message, &messages[i + 1..], &mut contents);
        }
    }
    (system_parts, contents)
}

/// A user message's parts, or a mid-conversation system message's, whose
/// text is wrapped as a reminder. The parts that can't be sent are recorded
/// in `drops`.
fn user_parts(content: Option<&Value>, demoted: bool, drops: &mut UserTurnDrops) -> Vec<Value> {
    let text = |text: &str| text_part(demoted_system_text(text.to_owned(), demoted));
    match content {
        Some(Value::String(content)) => vec![text(content)],
        Some(block @ Value::Object(_)) if str_of(block.get("type")) == "text" => {
            vec![text(&str_of(block.get("text")))]
        }
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                let kind = str_of(item.get("type"));
                let part = match kind.as_ref() {
                    "text" => {
                        let content = str_of(item.get("text"));
                        return (!content.is_empty()).then(|| text(&content));
                    }
                    // Only a base64 data URL can be inlined; a remote URL has
                    // no equivalent here.
                    "image_url" => data_url_file_part(&str_of(path(item, "image_url.url"))),
                    "video_url" => data_url_file_part(&str_of(path(item, "video_url.url"))),
                    "file" => normalize_openai_file_data(
                        &str_of(path(item, "file.filename")),
                        "",
                        &str_of(path(item, "file.file_data")),
                    )
                    .map(|file| inline_data_part(file.mime_type, file.data)),
                    "input_audio" => {
                        let data = str_of(path(item, "input_audio.data"));
                        (!data.is_empty()).then(|| {
                            let format = str_of(path(item, "input_audio.format"));
                            inline_data_part(input_audio_mime_type(&format), data.into_owned())
                        })
                    }
                    _ => return None,
                };
                if part.is_none() {
                    drops.drop_part(&kind);
                }
                part
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// An assistant message: one model turn of its reasoning, content and tool
/// calls, then a user turn answering the calls from the `tool` messages in
/// `rest`, up to the next assistant message.
fn convert_assistant(message: &Value, rest: &[Value], contents: &mut Vec<Value>) {
    let mut parts = Vec::new();
    if let Some(Value::String(reasoning)) = message.get("reasoning_content")
        && !reasoning.is_empty()
    {
        parts.push(json!({"text": reasoning, "thought": true}));
    }
    match message.get("content") {
        Some(Value::String(text)) if !text.is_empty() => parts.push(text_part(text.clone())),
        Some(Value::Array(items)) => {
            parts.extend(
                items
                    .iter()
                    .filter_map(|item| match str_of(item.get("type")).as_ref() {
                        "text" => {
                            let text = str_of(item.get("text"));
                            (!text.is_empty()).then(|| text_part(text.into_owned()))
                        }
                        "image_url" => data_url_part(&str_of(path(item, "image_url.url"))),
                        _ => None,
                    }),
            );
        }
        _ => {}
    }

    let Some(Value::Array(tool_calls)) = message.get("tool_calls") else {
        if !parts.is_empty() {
            contents.push(content_node("model", parts));
        }
        return;
    };

    // Each call's ID and sanitized name.
    let mut calls = Vec::new();
    for call in tool_calls {
        if str_of(call.get("type")) != "function" {
            continue;
        }
        let name = sanitize_gemini_function_name(&str_of(path(call, "function.name")));
        if name.is_empty() {
            continue;
        }
        let mut function_call = object([("name", Value::String(name.clone()))]);
        if let Ok(args) = serde_json::from_str::<Value>(&str_of(path(call, "function.arguments"))) {
            function_call["args"] = args;
        }
        parts.push(object([
            ("functionCall", function_call),
            (
                "thoughtSignature",
                Value::String(tool_call_thought_signature(call)),
            ),
        ]));
        calls.push((str_of(call.get("id")), name));
    }
    if !parts.is_empty() {
        contents.push(content_node("model", parts));
    }

    // The content of each tool message by call ID, the last one counting.
    let mut results: HashMap<Cow<'_, str>, Option<&Value>> = HashMap::new();
    for next in rest {
        let role = str_of(next.get("role"));
        if role == "assistant" {
            break;
        }
        if role == "tool" {
            let id = str_of(next.get("tool_call_id"));
            if !id.is_empty() {
                results.insert(id, next.get("content"));
            }
        }
    }

    let responses: Vec<Value> = calls
        .into_iter()
        .map(|(id, name)| {
            let result = match results.get(&id) {
                Some(Some(content)) => content.to_string(),
                _ => "{}".to_owned(),
            };
            json!({"functionResponse": {"name": name, "response": {"result": result}}})
        })
        .collect();
    if !responses.is_empty() {
        contents.push(content_node("user", responses));
    }
}

/// `openAIToolCallGeminiThoughtSignature`: the signature to send with a tool
/// call, from the first place one is given, or the validator bypass.
fn tool_call_thought_signature(call: &Value) -> String {
    TOOL_CALL_SIGNATURE_PATHS
        .iter()
        .find_map(|at| path(call, at))
        .map_or_else(
            || GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_owned(),
            |signature| {
                gemini_replay_signature_or_bypass(
                    &str_of(Some(signature)),
                    BlockKind::GeminiFunctionCall,
                )
            },
        )
}

/// The inline data part for a base64 `data:` URL, read as
/// `NormalizeOpenAIFileData` reads it. `None` for anything else.
fn data_url_file_part(url: &str) -> Option<Value> {
    normalize_openai_file_data("", "", url).map(|file| inline_data_part(file.mime_type, file.data))
}

/// The inline data part for a `data:` URL, read as upstream reads it: the
/// MIME type is what follows `data:` up to the first `;`, and the data what
/// follows the next seven bytes (`base64,`), whatever they are. `None` if
/// there is no `;`, or nothing after those seven bytes.
fn data_url_part(url: &str) -> Option<Value> {
    let rest = url.as_bytes().get(5..).filter(|rest| !rest.is_empty())?;
    let semicolon = rest.iter().position(|&b| b == b';')?;
    let data = rest[semicolon + 1..]
        .get(7..)
        .filter(|data| !data.is_empty())?;
    // Cutting at a byte offset can leave stray continuation bytes at the
    // start, which Go writes as U+FFFD each, as from_utf8_lossy does.
    Some(inline_data_part(
        String::from_utf8_lossy(&rest[..semicolon]).into_owned(),
        String::from_utf8_lossy(data).into_owned(),
    ))
}

/// `openAIInputAudioMimeType`.
fn input_audio_mime_type(format: &str) -> String {
    match format {
        "" | "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "webm" => "audio/webm",
        "pcm16" => "audio/pcm",
        "g711_ulaw" | "g711_alaw" => "audio/basic",
        _ => return format!("audio/{format}"),
    }
    .to_owned()
}

/// `geminiDemotedSystemText`: a mid-conversation system message's text
/// wrapped as a reminder, unless it is blank.
fn demoted_system_text(text: String, demoted: bool) -> String {
    if !demoted || text.trim().is_empty() {
        text
    } else {
        system_reminder_text(&text)
    }
}

fn text_part(text: String) -> Value {
    object([("text", Value::String(text))])
}

fn inline_data_part(mime_type: String, data: String) -> Value {
    object([(
        "inlineData",
        object([
            ("mime_type", Value::String(mime_type)),
            ("data", Value::String(data)),
        ]),
    )])
}

fn content_node(role: &str, parts: Vec<Value>) -> Value {
    object([("role", Value::from(role)), ("parts", Value::Array(parts))])
}

/// gjson `Array()`: an array's items, nothing for a missing value or `null`,
/// and any other value alone.
fn gjson_array(value: Option<&Value>) -> &[Value] {
    match value {
        None | Some(Value::Null) => &[],
        Some(Value::Array(items)) => items,
        Some(value) => std::slice::from_ref(value),
    }
}

/// A `tool_choice` of type `allowed_tools`: the names of the tools the model
/// may call, and the mode.
struct AllowedTools {
    names: HashSet<String>,
    mode: String,
}

impl AllowedTools {
    fn read(choice: Option<&Value>) -> Option<Self> {
        let choice = choice.filter(|choice| choice.is_object())?;
        if str_of(choice.get("type")) != "allowed_tools" {
            return None;
        }
        let mut tools = gjson_array(path(choice, "allowed_tools.tools"));
        if tools.is_empty() {
            tools = gjson_array(choice.get("tools"));
        }
        let names = tools
            .iter()
            .filter_map(|tool| {
                let name = str_of(path(tool, "function.name"));
                let name = match name.trim() {
                    "" => str_of(tool.get("name")).trim().to_owned(),
                    name => name.to_owned(),
                };
                (!name.is_empty()).then_some(name)
            })
            .collect();
        let mut mode = go::to_lower(str_of(path(choice, "allowed_tools.mode")).trim());
        if mode.is_empty() {
            mode = go::to_lower(str_of(choice.get("mode")).trim());
        }
        if mode.is_empty() {
            mode = "auto".to_owned();
        }
        Some(Self { names, mode })
    }
}

/// The function declarations, and what `tool_choice` needs to know of them.
#[derive(Default)]
struct Tools {
    declarations: Vec<Value>,
    has_strict: bool,
    /// Each declared tool's sanitized name, by its name as given.
    sanitized: HashMap<String, String>,
    /// How many declared tools have each sanitized name.
    counts: HashMap<String, usize>,
}

/// Writes `tools`: one holding the function declarations, then each Google
/// search, code execution and URL context tool, passed through. With
/// `allowed`, functions not named there are left out.
fn convert_tools(out: &mut Value, tools: Option<&Value>, allowed: Option<&AllowedTools>) -> Tools {
    let mut converted = Tools::default();
    let Some(Value::Array(tools)) = tools else {
        return converted;
    };
    let mut builtin: [Vec<Value>; 3] = Default::default();
    for tool in tools {
        if str_of(tool.get("type")) == "function"
            && let Some(Value::Object(function)) = tool.get("function")
        {
            let name = str_of(function.get("name")).into_owned();
            if allowed.is_some_and(|allowed| !allowed.names.contains(&name)) {
                continue;
            }
            let sanitized = sanitize_gemini_function_name(&name);
            *converted.counts.entry(sanitized.clone()).or_default() += 1;
            converted.sanitized.insert(name.clone(), sanitized.clone());
            let Some(declaration) =
                function_declaration(function, tool, &name, sanitized, &mut converted.has_strict)
            else {
                continue;
            };
            converted.declarations.push(declaration);
        }
        for (list, (from, to)) in builtin.iter_mut().zip([
            ("google_search", "googleSearch"),
            ("code_execution", "codeExecution"),
            ("url_context", "urlContext"),
        ]) {
            if let Some(config) = tool.get(from) {
                list.push(object([(to, config.clone())]));
            }
        }
    }

    let mut items = Vec::new();
    if !converted.declarations.is_empty() {
        let declarations = Value::Array(converted.declarations.clone());
        items.push(object([("functionDeclarations", declarations)]));
    }
    items.extend(builtin.into_iter().flatten());
    if !items.is_empty() {
        out["tools"] = Value::Array(items);
    }
    converted
}

/// A function's declaration: `parameters` renamed `parametersJsonSchema`
/// and cleaned, the name sanitized, and `strict` dropped (noted in
/// `has_strict` if true, or if the tool says so). With no `parameters`, the
/// schema is made an object with no properties; `None` if a
/// `parametersJsonSchema` list is in the way.
fn function_declaration(
    function: &Map<String, Value>,
    tool: &Value,
    name: &str,
    sanitized: String,
    has_strict: &mut bool,
) -> Option<Value> {
    let mut declaration = function.clone();
    if let Some(parameters) = declaration.shift_remove("parameters") {
        declaration.insert("parametersJsonSchema".to_owned(), parameters);
    } else {
        let schema = declaration
            .entry("parametersJsonSchema")
            .or_insert_with(|| Value::Object(Map::new()));
        if schema.is_array() {
            return None;
        }
        if !schema.is_object() {
            *schema = Value::Object(Map::new());
        }
        schema["type"] = Value::from("object");
        schema["properties"] = json!({});
    }
    if !matches!(declaration.get("name"), Some(Value::String(_))) || sanitized != name {
        declaration.insert("name".to_owned(), Value::String(sanitized));
    }
    if let Some(schema) = declaration.get_mut("parametersJsonSchema") {
        *schema = clean_json_schema_for_gemini_json_schema(schema);
    }
    if let Some(strict) = declaration.get("strict").or_else(|| tool.get("strict")) {
        *has_strict |= *strict == Value::Bool(true);
        declaration.shift_remove("strict");
    }
    Some(Value::Object(declaration))
}

/// Writes `toolConfig`'s function calling mode from `tool_choice`, failing
/// closed to `NONE` where it can't be honoured.
fn apply_tool_config(
    out: &mut Value,
    request: &Value,
    tools: &Tools,
    allowed: Option<&AllowedTools>,
) {
    let mode = |out: &mut Value, mode: &str| {
        set_path(out, MODE, Value::from(mode));
    };
    let declared = !tools.declarations.is_empty();

    if tools.counts.values().any(|&count| count > 1) {
        // Two tools sanitize alike, so a call can't be told apart.
        mode(out, "NONE");
    } else if let Some(allowed) = allowed {
        if !declared {
            mode(out, "NONE");
        } else if allowed.mode == "required" || allowed.mode == "any" {
            mode(out, "ANY");
            let names: Vec<Value> = tools
                .declarations
                .iter()
                .map(|declaration| Value::String(str_of(declaration.get("name")).into_owned()))
                .collect();
            set_path(out, ALLOWED_FUNCTION_NAMES, Value::Array(names));
        } else if tools.has_strict {
            mode(out, "VALIDATED");
        } else {
            mode(out, "AUTO");
        }
    } else if let Some(choice) = request
        .get("tool_choice")
        .filter(|choice| !choice.is_null())
    {
        let kind = match choice {
            Value::String(kind) => go::to_lower(kind.trim()),
            Value::Object(_) => go::to_lower(str_of(choice.get("type")).trim()),
            _ => String::new(),
        };
        match kind.as_str() {
            "auto" if tools.has_strict => mode(out, "VALIDATED"),
            "auto" => mode(out, "AUTO"),
            "required" | "any" => mode(out, "ANY"),
            "function" | "tool" => {
                let name = str_of(path(choice, "function.name"));
                let name = match name.trim() {
                    "" => str_of(choice.get("name")).trim().to_owned(),
                    name => name.to_owned(),
                };
                match tools.sanitized.get(&name) {
                    Some(sanitized) if tools.counts.get(sanitized) == Some(&1) => {
                        mode(out, "ANY");
                        set_path(out, ALLOWED_FUNCTION_NAMES, json!([sanitized]));
                    }
                    _ => mode(out, "NONE"),
                }
            }
            // "none", and anything unknown.
            _ => mode(out, "NONE"),
        }
    } else if tools.has_strict && declared {
        mode(out, "VALIDATED");
    }

    // Gemini can't turn off parallel calls alone, so this turns calls off.
    if request.get("parallel_tool_calls") == Some(&Value::Bool(false)) {
        mode(out, "NONE");
        delete_path(out, ALLOWED_FUNCTION_NAMES);
    }
}

#[cfg(test)]
mod tests;
