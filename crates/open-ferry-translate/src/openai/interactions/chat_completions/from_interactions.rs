// Ported from CLIProxyAPI internal/translator/openai/interactions/chat-completions/interactions_openai_request.go
// (ConvertInteractionsRequestToOpenAI) and openai_interactions_response.go
// (ConvertInteractionsResponseToOpenAI, ConvertInteractionsResponseToOpenAINonStream)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! From Interactions: an Interactions request becomes a Chat Completions
//! request, and an Interactions response or event stream becomes a Chat
//! Completions response or chunk stream.
//!
//! The request's `system_instruction` becomes a system message, and its
//! `input` steps become messages: user and model turns with their text or
//! parts, a `thought` as an assistant message's `reasoning_content`, a
//! `function_call` as an assistant tool call, and a `function_result` as a
//! tool message. Tools, including Gemini-style `function_declarations`,
//! become function tools, and the `generation_config` settings the request's
//! own.
//!
//! The stream opens with a chunk giving the role, then sends reasoning, text
//! and each tool call (indexed from 0 in the order they start) as they come,
//! and finishes once, at `interaction.completed` or `finish`, with
//! `tool_calls` if a tool was called. A failed interaction gives an error
//! object.
//!
//! Deviations from upstream: see the [module](super).

use std::collections::HashMap;

use serde_json::{Map, Value};

use super::common::{
    chat_usage_from_interactions, each, first_existing, first_non_empty, int_at, json_string_value,
    sse_payload, text_at, unix_nanos, unix_seconds,
};
use crate::common::interactions_usage::interactions_usage;
use crate::go;
use crate::json::{bool_of, exact, object, path, str_of};

/// `ConvertInteractionsRequestToOpenAI`: an Interactions request as a Chat
/// Completions request for `model_name`, or for the request's own model if
/// that is blank. `stream`, or the request's own `stream`, sets `stream`.
pub fn convert_interactions_request_to_openai(
    model_name: &str,
    root: &Value,
    stream: bool,
) -> Value {
    let mut out = Map::new();
    let model = first_non_empty(&[model_name, &text_at(root, "model")]);
    out.insert("model".into(), model.into());
    out.insert("messages".into(), Value::Array(Vec::new()));
    if stream || root.get("stream").is_some_and(bool_of) {
        out.insert("stream".into(), true.into());
    }
    let mut messages = Vec::new();
    // `appendInteractionsSystemToOpenAI`.
    let system = interactions_text(root.get("system_instruction"));
    if !system.is_empty() {
        messages.push(object([
            ("role", "system".into()),
            ("content", system.into()),
        ]));
    }
    append_input(&mut messages, root.get("input"));
    out.insert("messages".into(), Value::Array(messages));
    copy_tools(&mut out, root);
    copy_generation_config(&mut out, root);
    copy_top_level(&mut out, root);
    Value::Object(out)
}

/// `appendInteractionsInputToOpenAIMessages`: a string `input` as a user
/// message, or each step of it as messages.
fn append_input(messages: &mut Vec<Value>, input: Option<&Value>) {
    match input {
        Some(Value::String(text)) => messages.push(object([
            ("role", "user".into()),
            ("content", text.as_str().into()),
        ])),
        Some(Value::Array(steps)) => {
            for step in steps {
                append_step(messages, step);
            }
        }
        Some(step @ Value::Object(_)) => append_step(messages, step),
        _ => {}
    }
}

/// `appendInteractionsStepToOpenAI`: the message a step gives, if any. A
/// step that is a string is a user message.
fn append_step(messages: &mut Vec<Value>, step: &Value) {
    match text_at(step, "type").as_ref() {
        "user_input" => messages.push(message(step, "user")),
        "model_output" => messages.push(message(step, "assistant")),
        // `appendInteractionsThoughtToOpenAI`.
        "thought" => messages.push(object([
            ("role", "assistant".into()),
            ("content", "".into()),
            (
                "reasoning_content",
                interactions_text(step.get("content")).into(),
            ),
        ])),
        // `appendInteractionsFunctionCallToOpenAI`.
        "function_call" => messages.push(object([
            ("role", "assistant".into()),
            ("content", "".into()),
            ("tool_calls", Value::Array(vec![tool_call(step)])),
        ])),
        // `appendInteractionsFunctionResultToOpenAI`.
        "function_result" => {
            let call_id = first_non_empty(&[&text_at(step, "call_id"), &text_at(step, "id")]);
            let result = first_existing(&[step.get("result"), step.get("output")]);
            messages.push(object([
                ("role", "tool".into()),
                ("tool_call_id", call_id.into()),
                ("content", json_string_value(result, "").into()),
            ]));
        }
        _ => {
            if let Value::String(text) = step {
                messages.push(object([
                    ("role", "user".into()),
                    ("content", text.as_str().into()),
                ]));
            }
        }
    }
}

/// `appendInteractionsMessageToOpenAI`: a turn as a message of `role`. Its
/// content is a string, or its parts: joined into one string if they are
/// all text, or else a list.
fn message(step: &Value, role: &str) -> Value {
    let content = match step.get("content") {
        Some(Value::String(text)) => text.as_str().into(),
        Some(content) => content_parts(content).unwrap_or_else(|| "".into()),
        None => "".into(),
    };
    object([("role", role.into()), ("content", content)])
}

/// `appendInteractionsContentToOpenAIMessage`: the content of a turn's
/// parts, or `None` if none of them converts.
fn content_parts(content: &Value) -> Option<Value> {
    let parts: Vec<Part> = match content {
        Value::Array(parts) => parts.iter().filter_map(content_part).collect(),
        Value::Object(_) => content_part(content).into_iter().collect(),
        _ => Vec::new(),
    };
    if parts.is_empty() {
        return None;
    }
    if parts.iter().all(|part| matches!(part, Part::Text(_))) {
        let text: String = parts
            .iter()
            .filter_map(|part| match part {
                Part::Text(text) => Some(text.as_str()),
                Part::Other(_) => None,
            })
            .collect();
        return Some(text.into());
    }
    Some(Value::Array(
        parts.into_iter().map(Part::into_value).collect(),
    ))
}

/// A converted content part.
enum Part {
    Text(String),
    Other(Value),
}

impl Part {
    fn into_value(self) -> Value {
        match self {
            Self::Text(text) => object([("type", "text".into()), ("text", text.into())]),
            Self::Other(value) => value,
        }
    }
}

/// `interactionsContentPartToOpenAI`: an Interactions content part as a
/// Chat Completions one. `None` for a part of a type it doesn't know.
fn content_part(part: &Value) -> Option<Part> {
    let mut part_type = text_at(part, "type");
    if part_type.is_empty() && part.get("text").is_some() {
        part_type = "text".into();
    }
    let value = match part_type.as_ref() {
        "text" => return Some(Part::Text(text_at(part, "text").into_owned())),
        "image" => object([
            ("type", "image_url".into()),
            (
                "image_url",
                object([(
                    "url",
                    media_data_url(part, "application/octet-stream").into(),
                )]),
            ),
        ]),
        "audio" => object([
            ("type", "input_audio".into()),
            (
                "input_audio",
                object([
                    ("data", text_at(part, "data").into()),
                    (
                        "format",
                        input_audio_format(&text_at(part, "mime_type")).into(),
                    ),
                ]),
            ),
        ]),
        "video" => object([
            ("type", "video_url".into()),
            (
                "video_url",
                object([("url", media_data_url(part, "video/mp4").into())]),
            ),
        ]),
        "document" | "file" => {
            let mut file = Map::new();
            let filename = first_non_empty(&[
                &text_at(part, "filename"),
                &file_name_from_mime(&text_at(part, "mime_type")),
            ]);
            file.insert("filename".into(), filename.into());
            let url = first_non_empty(&[&text_at(part, "file_url"), &text_at(part, "url")]);
            if url.is_empty() {
                file.insert("file_data".into(), text_at(part, "data").into());
            } else {
                file.insert("file_url".into(), url.into());
            }
            object([("type", "file".into()), ("file", Value::Object(file))])
        }
        _ => return None,
    };
    Some(Part::Other(value))
}

/// `interactionsMediaDataURL`: a part's URL, or its data as a `data:` URL
/// typed by its MIME type or else `fallback_mime_type`; empty if it has
/// neither.
fn media_data_url(part: &Value, fallback_mime_type: &str) -> String {
    let url = first_non_empty(&[
        &text_at(part, "image_url"),
        &text_at(part, "file_data"),
        &text_at(part, "url"),
    ]);
    if !url.is_empty() {
        return url;
    }
    let data = text_at(part, "data");
    if data.is_empty() {
        return String::new();
    }
    let mime_type = first_non_empty(&[&text_at(part, "mime_type"), fallback_mime_type]);
    format!("data:{mime_type};base64,{data}")
}

/// `openAIInputAudioFormatFromMIME`: the `input_audio` format of a MIME
/// type.
fn input_audio_format(mime_type: &str) -> &'static str {
    match go::to_lower(mime_type.trim()).as_str() {
        "audio/wav" | "audio/wave" | "audio/x-wav" => "wav",
        "audio/flac" => "flac",
        "audio/opus" | "audio/ogg" => "opus",
        "audio/pcm" | "audio/l16" => "pcm16",
        _ => "mp3",
    }
}

/// `openAIFileNameFromMIME`: a file name for a document of a MIME type.
fn file_name_from_mime(mime_type: &str) -> String {
    match go::to_lower(mime_type.trim()).as_str() {
        "application/pdf" => "document.pdf".to_owned(),
        "text/plain" => "document.txt".to_owned(),
        "text/csv" => "document.csv".to_owned(),
        "application/json" => "document.json".to_owned(),
        _ => match mime_type.split_once('/') {
            Some((_, suffix)) if !suffix.is_empty() => {
                format!("document.{}", suffix.replace('+', "."))
            }
            _ => "document.bin".to_owned(),
        },
    }
}

/// `copyInteractionsToolsToOpenAI`: each tool, and each of its
/// `function_declarations`, as a function tool.
fn copy_tools(out: &mut Map<String, Value>, root: &Value) {
    let Some(Value::Array(tools)) = root.get("tools") else {
        return;
    };
    let mut items = Vec::new();
    for tool in tools {
        items.extend(function_tool(tool));
        let declarations = first_existing(&[
            tool.get("function_declarations"),
            tool.get("functionDeclarations"),
        ]);
        if let Some(Value::Array(declarations)) = declarations {
            items.extend(declarations.iter().filter_map(function_tool));
        }
    }
    if !items.is_empty() {
        out.insert("tools".into(), Value::Array(items));
    }
}

/// `openAIToolFromInteractionsTool`: a tool or function declaration as a
/// function tool. `None` if it has no name.
fn function_tool(tool: &Value) -> Option<Value> {
    let name = first_non_empty(&[&text_at(tool, "name"), &text_at(tool, "function.name")]);
    if name.is_empty() {
        return None;
    }
    let mut function = Map::new();
    function.insert("name".into(), name.into());
    let description =
        first_existing(&[tool.get("description"), path(tool, "function.description")]);
    if let Some(description) = description {
        function.insert("description".into(), str_of(Some(description)).into());
    }
    let parameters = first_existing(&[
        tool.get("parameters"),
        path(tool, "function.parameters"),
        tool.get("parametersJsonSchema"),
    ]);
    if let Some(parameters) = parameters {
        function.insert("parameters".into(), parameters.clone());
    }
    Some(object([
        ("type", "function".into()),
        ("function", Value::Object(function)),
    ]))
}

/// `copyInteractionsGenerationConfigToOpenAI`: the `generation_config` (or
/// `generationConfig`) settings, or else the request's own, as the Chat
/// Completions ones.
fn copy_generation_config(out: &mut Map<String, Value>, root: &Value) {
    let generation = root
        .get("generation_config")
        .or_else(|| root.get("generationConfig"));
    let setting = |key: &str| generation.and_then(|generation| path(generation, key));
    let fields = [
        (
            "temperature",
            first_existing(&[setting("temperature"), root.get("temperature")]),
        ),
        (
            "max_tokens",
            first_existing(&[
                setting("max_output_tokens"),
                setting("maxOutputTokens"),
                root.get("max_tokens"),
                root.get("max_completion_tokens"),
            ]),
        ),
        (
            "top_p",
            first_existing(&[setting("top_p"), setting("topP"), root.get("top_p")]),
        ),
        (
            "top_k",
            first_existing(&[setting("top_k"), setting("topK")]),
        ),
        (
            "n",
            first_existing(&[
                setting("candidate_count"),
                setting("candidateCount"),
                root.get("n"),
            ]),
        ),
        (
            "stop",
            first_existing(&[
                setting("stop_sequences"),
                setting("stopSequences"),
                root.get("stop"),
            ]),
        ),
        (
            "tool_choice",
            first_existing(&[setting("tool_choice"), root.get("tool_choice")]),
        ),
    ];
    for (key, value) in fields {
        if let Some(value) = value {
            out.insert(key.into(), value.clone());
        }
    }
    // `interactionsReasoningEffort`: the first of these that is a string,
    // even a blank one.
    let effort = [
        setting("reasoning_effort"),
        setting("thinking_level"),
        setting("thinkingLevel"),
        setting("thinking_config.thinking_level"),
        setting("thinkingConfig.thinkingLevel"),
        root.get("reasoning_effort"),
    ]
    .into_iter()
    .find_map(|value| value.and_then(Value::as_str))
    .map(|effort| go::to_lower(effort.trim()))
    .unwrap_or_default();
    if !effort.is_empty() {
        out.insert("reasoning_effort".into(), effort.into());
    }
    if let Some(modalities) = root.get("response_modalities") {
        out.insert("modalities".into(), modalities.clone());
    }
}

/// `copyInteractionsOpenAITopLevel`: the response format, service tier,
/// previous interaction, environment, agent config and the fields both
/// formats share.
fn copy_top_level(out: &mut Map<String, Value>, root: &Value) {
    if let Some(format) = root.get("response_format") {
        out.insert("response_format".into(), format.clone());
    }
    if let Some(Value::String(tier)) = root.get("service_tier") {
        out.insert("service_tier".into(), tier.as_str().into());
    }
    let previous = first_non_empty(&[
        &text_at(root, "previous_interaction_id"),
        &text_at(root, "previous_response_id"),
    ]);
    if !previous.is_empty() {
        out.insert("previous_response_id".into(), previous.into());
    }
    let environment = first_non_empty(&[
        &text_at(root, "environment_id"),
        &text_at(root, "environment.id"),
    ]);
    if !environment.is_empty() {
        out.insert("environment_id".into(), environment.into());
    }
    for key in ["agent_config", "parallel_tool_calls", "seed", "user"] {
        if let Some(value) = root.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
}

/// `interactionsText`: the text of a `system_instruction` or a thought's
/// content: a string, an object's `text`, or else the texts of its first
/// list of `content` or `parts`.
fn interactions_text(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    if let Value::String(text) = value {
        return text.clone();
    }
    if let Some(text) = value.get("text") {
        return str_of(Some(text)).into_owned();
    }
    for key in ["content", "parts"] {
        if let Some(Value::Array(parts)) = value.get(key) {
            return parts
                .iter()
                .map(|part| {
                    first_non_empty(&[&text_at(part, "text"), &text_at(part, "content.text")])
                })
                .collect();
        }
    }
    String::new()
}

/// `openAIChatToolCallFromInteractions`: a `function_call` step as a Chat
/// Completions tool call, its arguments as JSON text.
fn tool_call(step: &Value) -> Value {
    let id = first_non_empty(&[&text_at(step, "call_id"), &text_at(step, "id"), "call_0"]);
    object([
        ("id", id.into()),
        ("type", "function".into()),
        (
            "function",
            object([
                ("name", text_at(step, "name").into()),
                (
                    "arguments",
                    json_string_value(step.get("arguments"), "{}").into(),
                ),
            ]),
        ),
    ])
}

/// `ConvertInteractionsResponseToOpenAI`: translates an Interactions event
/// stream into Chat Completions chunks, one event at a time. Make one per
/// response.
pub struct InteractionsToOpenAIStream {
    /// The model the client asked for.
    model_name: String,
    /// The interaction's ID, once `interaction.created` gives it.
    id: String,
    /// The model the chunks name: the interaction's, or else `model_name`.
    model: String,
    environment_id: String,
    /// The Unix time of the first chunk, in seconds.
    created: i64,
    started: bool,
    completed: bool,
    saw_tool_call: bool,
    /// Each tool call's index in the chunks, by its step's index.
    tool_call_index_by_step: HashMap<i64, i64>,
    next_tool_call_index: i64,
}

impl InteractionsToOpenAIStream {
    /// `model` is the model the client asked for.
    pub fn new(model: &str) -> Self {
        Self {
            model_name: model.to_owned(),
            id: String::new(),
            model: model.to_owned(),
            environment_id: String::new(),
            created: 0,
            started: false,
            completed: false,
            saw_tool_call: false,
            tool_call_index_by_step: HashMap::new(),
            next_tool_call_index: 0,
        }
    }

    /// Translates one event, a `data:` line, an SSE frame or its payload,
    /// into the chunks it gives.
    pub fn translate(&mut self, event: &[u8]) -> Vec<Value> {
        self.model = first_non_empty(&[&self.model, &self.model_name]);
        let payload = sse_payload(event);
        if payload.is_empty() || go::trim_space(&payload) == b"[DONE]" {
            return Vec::new();
        }
        let root = exact::from_slice(&payload).unwrap_or(Value::Null);
        let mut out = Vec::new();
        match text_at(&root, "event_type").as_ref() {
            "interaction.created" => {
                self.id = first_non_empty(&[&text_at(&root, "interaction.id"), &self.id]);
                self.model = first_non_empty(&[
                    &text_at(&root, "interaction.model"),
                    &self.model,
                    &self.model_name,
                ]);
                self.note_environment(&root);
                self.ensure_started(&mut out);
            }
            "step.start" => self.step_start(&mut out, &root),
            "step.delta" => self.step_delta(&mut out, &root),
            "interaction.completed" | "finish" => {
                self.note_environment(&root);
                self.complete(&mut out, &root);
            }
            "response.failed" | "interaction.failed" => out.push(failed(&root)),
            _ => {}
        }
        out
    }

    /// Keeps the environment the event names, if any.
    fn note_environment(&mut self, root: &Value) {
        let environment = first_non_empty(&[
            &text_at(root, "interaction.environment_id"),
            &text_at(root, "environment_id"),
            &text_at(root, "interaction.environment.id"),
            &text_at(root, "environment.id"),
        ]);
        if !environment.is_empty() {
            self.environment_id = environment;
        }
    }

    /// `interactionsStepStartToOpenAIChat`: a `function_call` step starts a
    /// tool call; other steps give nothing until their deltas.
    fn step_start(&mut self, out: &mut Vec<Value>, root: &Value) {
        self.ensure_started(out);
        if text_at(root, "step.type") != "function_call" {
            return;
        }
        self.saw_tool_call = true;
        let index = int_at(root, "index");
        let tool_call_index = match self.tool_call_index_by_step.get(&index) {
            Some(&existing) => existing,
            None => {
                let next = self.next_tool_call_index;
                self.tool_call_index_by_step.insert(index, next);
                self.next_tool_call_index += 1;
                next
            }
        };
        let id = first_non_empty(&[
            &text_at(root, "step.call_id"),
            &text_at(root, "step.id"),
            &format!("call_{tool_call_index}"),
        ]);
        // `openAIChatToolCallStartChunk`.
        let tool_call = object([
            ("index", tool_call_index.into()),
            ("id", id.into()),
            ("type", "function".into()),
            (
                "function",
                object([
                    ("name", text_at(root, "step.name").into()),
                    ("arguments", "".into()),
                ]),
            ),
        ]);
        let chunk = self.chunk(
            object([("tool_calls", Value::Array(vec![tool_call]))]),
            Value::Null,
        );
        out.push(chunk);
    }

    /// `interactionsStepDeltaToOpenAIChat`: a thought's text as
    /// `reasoning_content`, a tool call's arguments, or text as `content`.
    fn step_delta(&mut self, out: &mut Vec<Value>, root: &Value) {
        let index = int_at(root, "index");
        self.ensure_started(out);
        let delta = match text_at(root, "delta.type").as_ref() {
            "thought_summary" => {
                let text = first_non_empty(&[
                    &text_at(root, "delta.content.text"),
                    &text_at(root, "delta.text"),
                ]);
                if text.is_empty() {
                    return;
                }
                object([("reasoning_content", text.into())])
            }
            "arguments_delta" => {
                // `openAIChatToolCallArgumentsChunk`.
                let tool_call_index = self
                    .tool_call_index_by_step
                    .get(&index)
                    .copied()
                    .unwrap_or(index);
                let tool_call = object([
                    ("index", tool_call_index.into()),
                    (
                        "function",
                        object([("arguments", text_at(root, "delta.arguments").into())]),
                    ),
                ]);
                object([("tool_calls", Value::Array(vec![tool_call]))])
            }
            _ => {
                let text = text_at(root, "delta.text");
                if text.is_empty() {
                    return;
                }
                object([("content", text.into())])
            }
        };
        let chunk = self.chunk(delta, Value::Null);
        out.push(chunk);
    }

    /// `ensureOpenAIChatStarted`: the first chunk, giving the role, once.
    fn ensure_started(&mut self, out: &mut Vec<Value>) {
        if self.started {
            return;
        }
        let chunk = self.chunk(object([("role", "assistant".into())]), Value::Null);
        self.started = true;
        out.push(chunk);
    }

    /// `appendOpenAIChatCompleted`: the finishing chunk, once, with the
    /// usage.
    fn complete(&mut self, out: &mut Vec<Value>, root: &Value) {
        if self.completed {
            return;
        }
        self.ensure_started(out);
        let default = if self.saw_tool_call {
            "tool_calls"
        } else {
            "stop"
        };
        let finish = finish_reason(root, root.get("interaction"), default);
        let mut chunk = self.chunk(Value::Object(Map::new()), finish.into());
        chat_usage_from_interactions(&mut chunk, "usage", interactions_usage(root));
        self.completed = true;
        out.push(chunk);
    }

    /// `openAIChatBaseChunk`: a chunk with `delta` and `finish_reason`.
    fn chunk(&mut self, delta: Value, finish_reason: Value) -> Value {
        let id = first_non_empty(&[&self.id, &format!("chatcmpl_{}", unix_nanos())]);
        // `openAIChatCreated`.
        if self.created == 0 {
            self.created = unix_seconds();
        }
        let mut chunk = Map::new();
        chunk.insert("id".into(), id.into());
        chunk.insert("object".into(), "chat.completion.chunk".into());
        chunk.insert("created".into(), self.created.into());
        chunk.insert("model".into(), self.model.as_str().into());
        chunk.insert(
            "choices".into(),
            Value::Array(vec![object([
                ("index", 0.into()),
                ("delta", delta),
                ("finish_reason", finish_reason),
            ])]),
        );
        if !self.environment_id.is_empty() {
            chunk.insert("environment_id".into(), self.environment_id.as_str().into());
        }
        Value::Object(chunk)
    }
}

/// The finish reason of a finished interaction: `content_filter` or `length`
/// when the interaction says so, or else `default`. `interaction` is where
/// the interaction keeps its own fields, read before the event's.
fn finish_reason(root: &Value, interaction: Option<&Value>, default: &'static str) -> &'static str {
    let field = |key: &str| {
        first_non_empty(&[
            &interaction.map_or_else(String::new, |interaction| {
                text_at(interaction, key).into_owned()
            }),
            &text_at(root, key),
        ])
    };
    let reason = field("finish_reason");
    if reason == "content_filter" {
        "content_filter"
    } else if field("status") == "incomplete" || reason == "length" || reason == "max_tokens" {
        "length"
    } else {
        default
    }
}

/// `interactionsFailedToOpenAIChat`: a failed interaction's error as a Chat
/// Completions error object.
fn failed(root: &Value) -> Value {
    let error = root
        .get("error")
        .or_else(|| path(root, "interaction.error"));
    let field =
        |key: &str| error.map_or_else(String::new, |error| text_at(error, key).into_owned());
    let mut message = field("message");
    if message.is_empty() {
        message = "upstream error occurred".to_owned();
    }
    let mut error_type = field("type");
    if error_type.is_empty() {
        error_type = "server_error".to_owned();
    }
    let code = field("code");
    let mut out = Map::new();
    out.insert("message".into(), message.into());
    out.insert("type".into(), error_type.into());
    if !code.is_empty() {
        out.insert("code".into(), code.into());
    }
    object([("error", Value::Object(out))])
}

/// `ConvertInteractionsResponseToOpenAINonStream`: an Interactions response,
/// or an event holding one in `interaction`, as a Chat Completions response.
pub fn convert_interactions_response_to_openai_non_stream(model_name: &str, root: &Value) -> Value {
    let interaction = root.get("interaction").unwrap_or(root);
    let id = first_non_empty(&[
        &text_at(interaction, "id"),
        &text_at(root, "id"),
        &format!("chatcmpl_{}", unix_nanos()),
    ]);
    let model = first_non_empty(&[&text_at(interaction, "model"), model_name]);
    let steps = interaction.get("steps").or_else(|| root.get("steps"));
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for step in each(steps) {
        match text_at(step, "type").as_ref() {
            "model_output" => text.extend(content_texts(step.get("content"))),
            "thought" => reasoning.extend(content_texts(step.get("content"))),
            "function_call" => tool_calls.push(tool_call(step)),
            _ => {}
        }
    }
    let saw_tool_call = !tool_calls.is_empty();

    let mut message = Map::new();
    message.insert("role".into(), "assistant".into());
    message.insert("content".into(), text.into());
    if !reasoning.is_empty() {
        message.insert("reasoning_content".into(), reasoning.into());
    }
    if saw_tool_call {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
        message.insert("content".into(), Value::Null);
    }
    let default = if saw_tool_call { "tool_calls" } else { "stop" };
    let finish = finish_reason(root, Some(interaction), default);

    let mut out = Map::new();
    out.insert("id".into(), id.into());
    out.insert("object".into(), "chat.completion".into());
    out.insert("created".into(), unix_seconds().into());
    out.insert("model".into(), model.into());
    out.insert(
        "choices".into(),
        Value::Array(vec![object([
            ("index", 0.into()),
            ("message", Value::Object(message)),
            ("finish_reason", finish.into()),
        ])]),
    );
    let environment = first_non_empty(&[
        &text_at(interaction, "environment_id"),
        &text_at(root, "environment_id"),
        &text_at(interaction, "environment.id"),
        &text_at(root, "environment.id"),
        &text_at(root, "interaction.environment_id"),
    ]);
    if !environment.is_empty() {
        out.insert("environment_id".into(), environment.into());
    }
    let mut out = Value::Object(out);
    chat_usage_from_interactions(&mut out, "usage", interactions_usage(root));
    out
}

/// `interactionsContentTextsForOpenAIChat`: the texts of a step's content, a
/// string or its parts' texts.
fn content_texts(content: Option<&Value>) -> Vec<String> {
    match content {
        None => Vec::new(),
        Some(Value::String(text)) => vec![text.clone()],
        content => each(content)
            .into_iter()
            .map(|part| first_non_empty(&[&text_at(part, "text"), &text_at(part, "content.text")]))
            .filter(|text| !text.is_empty())
            .collect(),
    }
}

#[cfg(test)]
mod tests;
