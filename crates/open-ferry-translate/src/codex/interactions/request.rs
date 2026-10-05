// Ported from CLIProxyAPI internal/translator/codex/interactions/interactions_codex_request.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini Interactions request → Codex (OpenAI Responses) request.
//!
//! The system instruction becomes `instructions`, the steps become `input`
//! items (one message for each content part), function declarations become
//! function tools, and the generation config's settings are copied to where
//! Codex reads them. A handful of top-level fields (`tool_choice`,
//! `parallel_tool_calls`, `store`, `metadata`, `include`, `truncation`) pass
//! through as the client wrote them.
//!
//! No client identity is added: like upstream, the request carries no
//! Originator, no session or conversation ID, no `prompt_cache_key` and no
//! `client_metadata`, so nothing had to be left out. A client's own
//! `prompt_cache_key` isn't copied either, as upstream doesn't copy it.
//!
//! Deviations from upstream:
//! - Where upstream copies the client's raw JSON, we write compact JSON. The
//!   values are the same JSON. In strings this applies to a call's
//!   `arguments` and a result's `output` that aren't strings, and to texts,
//!   names, IDs and media fields read from a value that isn't a string.
//! - The generation config's settings are copied in the order upstream lists
//!   them, where upstream walks a Go map, in an order that changes from run
//!   to run. So when two of them set the same field (`max_tokens` and
//!   `maxOutputTokens`, say), the last listed wins, and `verbosity` is set
//!   within `text` after `text` is copied; upstream's result varies. The
//!   fields come out in that order too.
//! - Tool names cut to 64 bytes are cut at a UTF-8 character boundary.
//!   Upstream slices bytes and can split a character.
//! - Where a key is repeated in an object, the last one counts. gjson reads
//!   the first.
//! - A thinking budget beyond `i64`'s range saturates, where Go's result
//!   depends on the CPU.

use serde_json::{Map, Value};

use super::super::unique_names::truncate_bytes;
use crate::go;
use crate::json::lenient::{self, Found};
use crate::json::{bool_of, int_of, object, path, set_path, str_of};
use crate::thinking::budget_to_level;

/// The Responses API limit on function names.
const NAME_LIMIT: usize = 64;

/// The generation config's settings copied as they are, each with the field
/// it sets.
const COPIED_SETTINGS: &[(&str, &str)] = &[
    ("max_output_tokens", "max_output_tokens"),
    ("maxOutputTokens", "max_output_tokens"),
    ("max_tokens", "max_output_tokens"),
    ("temperature", "temperature"),
    ("top_p", "top_p"),
    ("topP", "top_p"),
    ("presence_penalty", "presence_penalty"),
    ("presencePenalty", "presence_penalty"),
    ("frequency_penalty", "frequency_penalty"),
    ("frequencyPenalty", "frequency_penalty"),
    ("parallel_tool_calls", "parallel_tool_calls"),
    ("parallelToolCalls", "parallel_tool_calls"),
    ("response_format", "response_format"),
    ("responseFormat", "response_format"),
    ("text", "text"),
    ("verbosity", "text.verbosity"),
    ("truncation", "truncation"),
    ("tool_choice", "tool_choice"),
    ("toolChoice", "tool_choice"),
    ("service_tier", "service_tier"),
    ("serviceTier", "service_tier"),
];

/// Where the generation config names a reasoning level, in the order
/// they're tried.
const LEVEL_PATHS: &[&str] = &[
    "thinking_level",
    "thinkingLevel",
    "thinking_config.thinking_level",
    "thinking_config.thinkingLevel",
    "thinkingConfig.thinking_level",
    "thinkingConfig.thinkingLevel",
    "reasoning.effort",
];

/// Where it gives a thinking budget instead.
const BUDGET_PATHS: &[&str] = &[
    "thinking_budget",
    "thinkingBudget",
    "thinking_config.thinking_budget",
    "thinking_config.thinkingBudget",
    "thinkingConfig.thinking_budget",
    "thinkingConfig.thinkingBudget",
];

/// Where it asks for reasoning summaries by name.
const SUMMARY_PATHS: &[&str] = &[
    "thinking_summaries",
    "thinkingSummaries",
    "reasoning.summary",
];

/// Where it asks for them with a boolean.
const INCLUDE_THOUGHTS_PATHS: &[&str] = &[
    "include_thoughts",
    "includeThoughts",
    "thinking_config.include_thoughts",
    "thinking_config.includeThoughts",
    "thinkingConfig.include_thoughts",
    "thinkingConfig.includeThoughts",
];

/// The top-level fields that pass through.
const PASSED_THROUGH: &[&str] = &[
    "tool_choice",
    "parallel_tool_calls",
    "store",
    "metadata",
    "include",
    "truncation",
];

/// `ConvertInteractionsRequestToCodex`: converts an Interactions request body
/// into a Codex request body for `model_name`. The request streams if
/// `stream` is set or the body asks for it.
pub fn convert_interactions_request_to_codex(
    model_name: &str,
    request: &Value,
    stream: bool,
) -> Value {
    let mut out = object([
        ("model", model_name.into()),
        ("instructions", "".into()),
        ("input", Value::Array(Vec::new())),
    ]);
    if stream || request.get("stream").is_some_and(bool_of) {
        set_path(&mut out, "stream", true.into());
    }
    copy_system(&mut out, request);
    copy_generation_config(&mut out, request);
    let mut items = Vec::new();
    push_input(&mut items, request.get("input"));
    if !items.is_empty() {
        set_path(&mut out, "input", Value::Array(items));
    }
    copy_tools(&mut out, request);
    copy_top_level(&mut out, request);
    out
}

/// `copyInteractionsSystemToCodex`: the system instruction, a string, an
/// object with a string `text`, or one with `parts` whose texts are joined.
fn copy_system(out: &mut Value, request: &Value) {
    let Some(system) = request
        .get("system_instruction")
        .or_else(|| request.get("systemInstruction"))
    else {
        return;
    };
    let instructions = match system {
        Value::String(text) => Some(text.clone()),
        _ => match (system.get("text"), system.get("parts")) {
            (Some(Value::String(text)), _) => Some(text.clone()),
            (_, Some(Value::Array(parts))) => Some(join_texts(
                parts.iter().map(|part| str_of(part.get("text"))),
            ))
            .filter(|text| !text.is_empty()),
            _ => None,
        },
    };
    if let Some(instructions) = instructions {
        set_path(out, "instructions", instructions.into());
    }
}

/// `copyInteractionsGenerationConfigToCodex`. Without a generation config,
/// only a top-level `reasoning` is copied.
fn copy_generation_config(out: &mut Value, request: &Value) {
    let Some(config) = request
        .get("generation_config")
        .or_else(|| request.get("generationConfig"))
    else {
        if let Some(reasoning) = request.get("reasoning") {
            set_path(out, "reasoning", reasoning.clone());
        }
        return;
    };
    if let Some(reasoning) = config.get("reasoning") {
        set_path(out, "reasoning", reasoning.clone());
    }
    if let Some(effort) = reasoning_effort(config) {
        set_path(out, "reasoning.effort", effort.into());
    }
    if let Some(summary) = reasoning_summary(config) {
        set_path(out, "reasoning.summary", summary.into());
    }
    for &(source, target) in COPIED_SETTINGS {
        if let Some(value) = config.get(source) {
            set_path(out, target, value.clone());
        }
    }
}

/// `interactionsCodexReasoningEffort`: the first level named, lowercased,
/// else the level of the first budget that has one.
fn reasoning_effort(config: &Value) -> Option<String> {
    let level = LEVEL_PATHS.iter().find_map(|key| {
        let effort = go::to_lower(str_of(path(config, key)).trim());
        (!effort.is_empty()).then_some(effort)
    });
    level.or_else(|| {
        BUDGET_PATHS.iter().find_map(|key| {
            let budget = path(config, key)?;
            budget_to_level(int_of(budget)).map(str::to_owned)
        })
    })
}

/// `interactionsCodexReasoningSummary`: `auto` or `none`, named as a string,
/// else from the first `include_thoughts` that is a boolean.
fn reasoning_summary(config: &Value) -> Option<&'static str> {
    let named = SUMMARY_PATHS
        .iter()
        .find_map(|key| match path(config, key) {
            Some(Value::String(summary)) => match go::to_lower(summary.trim()).as_str() {
                "auto" => Some("auto"),
                "none" => Some("none"),
                _ => None,
            },
            _ => None,
        });
    named.or_else(|| {
        INCLUDE_THOUGHTS_PATHS
            .iter()
            .find_map(|key| match path(config, key) {
                Some(Value::Bool(true)) => Some("auto"),
                Some(Value::Bool(false)) => Some("none"),
                _ => None,
            })
    })
}

/// `appendInteractionsInputToCodex`: a string, a list of steps, an object
/// with `steps` (and a `role` for them), or a single step.
fn push_input(items: &mut Vec<Value>, input: Option<&Value>) {
    match input {
        None => {}
        Some(Value::String(text)) => push_text(items, "user", text),
        Some(Value::Array(steps)) => {
            for step in steps {
                push_step(items, step, "user");
            }
        }
        Some(input) => match input.get("steps") {
            Some(Value::Array(steps)) => {
                let role = default_role(&str_of(input.get("role")), "user");
                for step in steps {
                    push_step(items, step, role);
                }
            }
            _ => push_step(items, input, "user"),
        },
    }
}

/// `appendInteractionsStepToCodex`.
fn push_step(items: &mut Vec<Value>, step: &Value, role: &'static str) {
    if let Value::String(text) = step {
        push_text(items, role, text);
        return;
    }
    if let Some(Value::Array(steps)) = step.get("steps") {
        let role = default_role(&str_of(step.get("role")), role);
        for nested in steps {
            push_step(items, nested, role);
        }
        return;
    }
    match go::to_lower(str_of(step.get("type")).trim()).as_str() {
        "function_call" => items.push(function_call(step)),
        "function_result" | "function_call_output" => items.push(function_result(step)),
        "model_output" | "assistant" => push_content(items, step.get("content"), "assistant"),
        "thought" | "reasoning" => items.push(thought(step)),
        // `user_input`, `message`, no type, and any other.
        _ => {
            let role = default_role(&str_of(step.get("role")), role);
            if let Some(content) = step.get("content") {
                push_content(items, Some(content), role);
            } else if let Some(text) = step.get("text") {
                push_text(items, role, &str_of(Some(text)));
            }
        }
    }
}

/// `appendInteractionsContentToCodexItem`: a message for a string, and one
/// for each part that converts.
fn push_content(items: &mut Vec<Value>, content: Option<&Value>, role: &'static str) {
    match content {
        Some(Value::String(text)) => push_text(items, role, text),
        Some(Value::Array(parts)) => {
            for part in parts {
                if let Some(part) = message_part(part, role) {
                    push_message(items, role, part);
                }
            }
        }
        Some(part @ Value::Object(_)) => {
            if let Some(part) = message_part(part, role) {
                push_message(items, role, part);
            }
        }
        _ => {}
    }
}

/// `appendInteractionsFunctionCallToCodex`.
fn function_call(step: &Value) -> Value {
    let mut item = Map::new();
    item.insert("type".into(), "function_call".into());
    if let Some(name) = step.get("name") {
        item.insert("name".into(), shorten_name(&str_of(Some(name))).into());
    }
    let call_id = call_id(step);
    if !call_id.is_empty() {
        item.insert("call_id".into(), call_id.into());
    }
    if let Some(arguments) = step.get("arguments").or_else(|| step.get("args")) {
        item.insert("arguments".into(), json_text(arguments).into());
    }
    Value::Object(item)
}

/// `appendInteractionsFunctionResultToCodex`.
fn function_result(step: &Value) -> Value {
    let mut item = Map::new();
    item.insert("type".into(), "function_call_output".into());
    let call_id = call_id(step);
    if !call_id.is_empty() {
        item.insert("call_id".into(), call_id.into());
    }
    if let Some(output) = step.get("result").or_else(|| step.get("output")) {
        item.insert("output".into(), json_text(output).into());
    }
    Value::Object(item)
}

/// `appendInteractionsThoughtToCodex`.
fn thought(step: &Value) -> Value {
    let mut text = content_text(step.get("content"));
    if text.is_empty() {
        text = str_of(step.get("text")).into_owned();
    }
    let mut item = Map::new();
    item.insert("type".into(), "reasoning".into());
    if !text.is_empty() {
        item.insert("content".into(), text.into());
    }
    if let Some(id) = step.get("id") {
        item.insert("id".into(), str_of(Some(id)).into());
    }
    Value::Object(item)
}

/// `appendInteractionsTextToCodex`.
fn push_text(items: &mut Vec<Value>, role: &'static str, text: &str) {
    push_message(items, role, text_part(role, text));
}

/// `appendInteractionsMessagePartToCodex`: a message holding one part.
fn push_message(items: &mut Vec<Value>, role: &'static str, part: Value) {
    items.push(object([
        ("type", "message".into()),
        ("role", role.into()),
        ("content", Value::Array(vec![part])),
    ]));
}

/// A text part: output text from the assistant, input text from anyone
/// else.
fn text_part(role: &str, text: &str) -> Value {
    let kind = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    object([("type", kind.into()), ("text", text.into())])
}

/// `interactionsCodexMessagePart`: a content part as a Codex message part,
/// if it converts.
fn message_part(part: &Value, role: &str) -> Option<Value> {
    if let Some(text) = part.get("text") {
        return Some(text_part(role, &str_of(Some(text))));
    }
    match go::to_lower(str_of(part.get("type")).trim()).as_str() {
        "text" | "" => None,
        "image" => image_part(part),
        "image_url" => Some(input_image(
            str_of(path(part, "image_url.url")).into_owned(),
        )),
        "audio" => audio_part(part),
        "input_audio" => Some(object([
            ("type", "input_audio".into()),
            (
                "input_audio",
                part.get("input_audio")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Map::new())),
            ),
        ])),
        "video" | "document" | "file" => file_part(part),
        _ => {
            if let Some(inline) = part.get("inline_data").or_else(|| part.get("inlineData")) {
                inline_part(inline)
            } else if let Some(file) = part.get("file_data").or_else(|| part.get("fileData")) {
                file_data_part(file)
            } else {
                None
            }
        }
    }
}

fn input_image(url: String) -> Value {
    object([("type", "input_image".into()), ("image_url", url.into())])
}

/// `interactionsCodexImagePart`: a URL, a file URI, or inline data as a
/// data URL.
fn image_part(part: &Value) -> Option<Value> {
    if let Some(url) = part.get("url") {
        return Some(input_image(str_of(Some(url)).into_owned()));
    }
    let uri = first_string(part, &["file_uri", "fileUri"]);
    if !uri.is_empty() {
        return Some(input_image(uri));
    }
    let mime_type = first_string(part, &["mime_type", "mimeType"]);
    let data = str_of(part.get("data"));
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    Some(input_image(format!("data:{mime_type};base64,{data}")))
}

/// `interactionsCodexAudioPart`: inline audio data.
fn audio_part(part: &Value) -> Option<Value> {
    let mime_type = first_string(part, &["mime_type", "mimeType"]);
    let data = str_of(part.get("data"));
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    Some(object([
        ("type", "input_audio".into()),
        (
            "input_audio",
            object([
                ("data", data.into_owned().into()),
                ("format", audio_format(&mime_type).into()),
            ]),
        ),
    ]))
}

/// `interactionsCodexFilePart`: a `file` object's data, a file URI or URL,
/// or inline data.
fn file_part(part: &Value) -> Option<Value> {
    let file_data = str_of(path(part, "file.file_data"));
    if !file_data.is_empty() {
        return Some(object([
            ("type", "input_file".into()),
            ("file_data", file_data.into_owned().into()),
            (
                "filename",
                str_of(path(part, "file.filename")).into_owned().into(),
            ),
        ]));
    }
    let mime_type = first_string(part, &["mime_type", "mimeType"]);
    let uri = first_string(part, &["file_uri", "fileUri", "url"]);
    if !uri.is_empty() {
        return Some(object([
            ("type", "input_file".into()),
            ("file_url", uri.into()),
            ("filename", file_name(&mime_type).into()),
        ]));
    }
    let data = str_of(part.get("data"));
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    Some(object([
        ("type", "input_file".into()),
        ("file_data", data.into_owned().into()),
        ("filename", file_name(&mime_type).into()),
    ]))
}

/// `interactionsCodexInlinePart`: inline data as an image, audio or file
/// part, by its MIME type.
///
/// Upstream builds the part to convert as text, quoting the MIME type and
/// the data with Go's `%q`, then reads it back with gjson. Where `%q` writes
/// an escape gjson doesn't know (`\a`, `\v`, `\x..` or `\U........`, for a
/// control character or an unprintable one beyond U+FFFF), gjson's value
/// stops there, so ours does too.
fn inline_part(inline: &Value) -> Option<Value> {
    let mime_type = first_string(inline, &["mime_type", "mimeType"]);
    let data = str_of(inline.get("data"));
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    let part = object([
        ("mime_type", go_quoted(&mime_type).into()),
        ("data", go_quoted(&data).into()),
    ]);
    let lower = go::to_lower(&mime_type);
    if lower.starts_with("image/") {
        image_part(&part)
    } else if lower.starts_with("audio/") {
        audio_part(&part)
    } else {
        file_part(&part)
    }
}

/// `text` quoted with Go's `%q`, then read back as gjson reads a string.
fn go_quoted(text: &str) -> String {
    lenient::get(&format!("{{\"v\":{}}}", go::quote(text)), "v")
        .map(Found::into_string)
        .unwrap_or_default()
}

/// `interactionsCodexFileDataPart`: a file URI, as an image or a file by its
/// MIME type.
fn file_data_part(file: &Value) -> Option<Value> {
    let mime_type = first_string(file, &["mime_type", "mimeType"]);
    let uri = first_string(file, &["file_uri", "fileUri"]);
    if uri.is_empty() {
        return None;
    }
    if go::to_lower(&mime_type).starts_with("image/") {
        return Some(input_image(uri));
    }
    Some(object([
        ("type", "input_file".into()),
        ("file_url", uri.into()),
        ("filename", file_name(&mime_type).into()),
    ]))
}

/// `copyInteractionsToolsToCodex`: function declarations, in a tool's
/// `function_declarations` or as the tool itself, become function tools.
/// If none do, the tools pass through as they are.
fn copy_tools(out: &mut Value, request: &Value) {
    let Some(tools) = request.get("tools") else {
        return;
    };
    let Value::Array(list) = tools else {
        set_path(out, "tools", tools.clone());
        return;
    };
    let mut converted = Vec::new();
    for tool in list {
        if let Some(declarations) = tool
            .get("function_declarations")
            .or_else(|| tool.get("functionDeclarations"))
        {
            if let Value::Array(declarations) = declarations {
                converted.extend(
                    declarations
                        .iter()
                        .filter(|declaration| declaration.get("name").is_some())
                        .map(function_tool),
                );
            }
        } else if tool.get("name").is_some() {
            converted.push(function_tool(tool));
        }
    }
    if converted.is_empty() {
        set_path(out, "tools", tools.clone());
        return;
    }
    set_path(out, "tools", Value::Array(converted));
    if out.get("tool_choice").is_none() {
        set_path(out, "tool_choice", "auto".into());
    }
}

/// `codexToolFromDeclaration`. Upstream builds a Go map and marshals it, so
/// the fields come out sorted by name.
fn function_tool(declaration: &Value) -> Value {
    let mut tool = Map::new();
    if let Some(description) = declaration.get("description") {
        tool.insert(
            "description".into(),
            str_of(Some(description)).into_owned().into(),
        );
    }
    tool.insert(
        "name".into(),
        shorten_name(&str_of(declaration.get("name"))).into(),
    );
    if let Some(parameters) = declaration
        .get("parameters")
        .or_else(|| declaration.get("parametersJsonSchema"))
        .or_else(|| declaration.get("parameters_json_schema"))
    {
        tool.insert("parameters".into(), cleaned_parameters(parameters));
    }
    tool.insert("strict".into(), false.into());
    tool.insert("type".into(), "function".into());
    Value::Object(tool)
}

/// `cleanedCodexToolParameters`: the schema without `$schema`, and with
/// `additionalProperties` false. sjson can't set a key in an array, so an
/// array stays as it is, and makes any other value that isn't an object an
/// object.
fn cleaned_parameters(parameters: &Value) -> Value {
    match parameters {
        Value::Object(schema) => {
            let mut schema = schema.clone();
            schema.shift_remove("$schema");
            if schema.get("additionalProperties") != Some(&Value::Bool(false)) {
                schema.insert("additionalProperties".into(), false.into());
            }
            Value::Object(schema)
        }
        Value::Array(_) => parameters.clone(),
        _ => object([("additionalProperties", false.into())]),
    }
}

/// `copyInteractionsCodexTopLevel`: a priority service tier, and the fields
/// that pass through. Upstream skips a field already holding the same text;
/// writing it again gives the same request.
fn copy_top_level(out: &mut Value, request: &Value) {
    if let Some(Value::String(tier)) = request.get("service_tier")
        && matches!(go::to_lower(tier.trim()).as_str(), "priority" | "fast")
    {
        set_path(out, "service_tier", "priority".into());
    }
    for &key in PASSED_THROUGH {
        if let Some(value) = request.get(key) {
            set_path(out, key, value.clone());
        }
    }
}

/// `interactionsCodexContentText`: a string, an object's `text`, or the
/// texts of a list of parts joined.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(part @ Value::Object(_)) => str_of(part.get("text")).into_owned(),
        Some(Value::Array(parts)) => join_texts(parts.iter().map(|part| str_of(part.get("text")))),
        _ => String::new(),
    }
}

/// The texts that aren't empty, a line each.
fn join_texts<'t>(texts: impl Iterator<Item = std::borrow::Cow<'t, str>>) -> String {
    let mut joined = String::new();
    for text in texts.filter(|text| !text.is_empty()) {
        if !joined.is_empty() {
            joined.push('\n');
        }
        joined.push_str(&text);
    }
    joined
}

/// `interactionsCodexCallID`: `call_id`, else `id`, trimmed.
fn call_id(step: &Value) -> String {
    let call_id = str_of(step.get("call_id"));
    if !call_id.trim().is_empty() {
        return call_id.trim().to_owned();
    }
    str_of(step.get("id")).trim().to_owned()
}

/// `interactionsCodexJSONString` and `interactionsCodexOutputString`: a
/// string as it is, anything else as JSON.
fn json_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        _ => value.to_string(),
    }
}

/// `firstString`: the first of `keys` that `value` has, as a string.
fn first_string(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| value.get(*key))
        .map(|found| str_of(Some(found)).into_owned())
        .unwrap_or_default()
}

/// `interactionsCodexDefaultRole`: the role named, else `fallback` if it is
/// the assistant or the developer, else the user.
fn default_role(role: &str, fallback: &'static str) -> &'static str {
    match go::to_lower(role.trim()).as_str() {
        "model" | "assistant" => "assistant",
        "developer" | "system" => "developer",
        "user" => "user",
        _ if matches!(fallback, "assistant" | "developer") => fallback,
        _ => "user",
    }
}

/// `codexInputAudioFormatFromMIME`.
fn audio_format(mime_type: &str) -> &'static str {
    match go::to_lower(mime_type.trim()).as_str() {
        "audio/wav" | "audio/wave" | "audio/x-wav" => "wav",
        "audio/flac" => "flac",
        "audio/opus" | "audio/ogg" => "opus",
        "audio/pcm" | "audio/l16" => "pcm16",
        _ => "mp3",
    }
}

/// `codexFileNameFromMIME`.
fn file_name(mime_type: &str) -> &'static str {
    match go::to_lower(mime_type.trim()).as_str() {
        "application/pdf" => "document.pdf",
        "text/plain" => "document.txt",
        "text/csv" => "document.csv",
        "application/json" => "document.json",
        "application/xml" | "text/xml" => "document.xml",
        lower if lower.starts_with("video/") => "video",
        _ => "document",
    }
}

/// `shortenCodexToolNameIfNeeded`: a name over 64 bytes is cut to them; an
/// `mcp__` one keeps `mcp__` and what follows its last `__`.
fn shorten_name(name: &str) -> String {
    if name.len() <= NAME_LIMIT {
        return name.to_owned();
    }
    if name.starts_with("mcp__")
        && let Some(index) = name.rfind("__")
        && index > 0
    {
        let candidate = format!("mcp__{}", name.get(index + 2..).unwrap_or_default());
        return truncate_bytes(&candidate, NAME_LIMIT).to_owned();
    }
    truncate_bytes(name, NAME_LIMIT).to_owned()
}
