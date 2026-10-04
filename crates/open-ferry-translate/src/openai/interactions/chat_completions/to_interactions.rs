// Ported from CLIProxyAPI internal/translator/openai/interactions/chat-completions/openai_interactions_request.go
// (ConvertOpenAIRequestToInteractions) and interactions_openai_response.go
// (ConvertOpenAIResponseToInteractions, ConvertOpenAIResponseToInteractionsNonStream)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Into Interactions: a Chat Completions request becomes an Interactions
//! request, and a Chat Completions response or chunk stream becomes an
//! Interactions response or event stream.
//!
//! The request's system and developer messages join, a line each, into the
//! `system_instruction`; the other messages become `input` steps: a
//! `thought` for each reasoning text, a `model_output` or `user_input` for
//! their content, a `function_call` for each tool call and a
//! `function_result` for each tool message, named after the call it answers
//! where the message names none. The sampling settings go into
//! `generation_config`, and function tools into `tools`.
//!
//! The stream opens with `interaction.created` and
//! `interaction.status_update`, then frames a step for each run of reasoning,
//! text or one tool call's arguments, and ends with `interaction.completed`
//! and `done` at `[DONE]`, or completes at a chunk with usage and no choices.
//!
//! Deviations from upstream: see the [module](super).

use std::collections::HashMap;

use serde_json::{Map, Value};

use super::common::{
    each, first_existing, first_non_empty, int_at, interactions_usage_from_chat, reasoning_texts,
    sse_payload, text_at, text_step, tool_call_step, unix_nanos, unix_seconds,
};
use crate::common::file_data::normalize_openai_file_data;
use crate::common::gemini_response::create_time;
use crate::common::sse::{push_event, push_frame};
use crate::go;
use crate::json::{bool_of, object, path, set_path, str_of};

/// `ConvertOpenAIRequestToInteractions`: a Chat Completions request as an
/// Interactions request for `model_name`, or for the request's own model if
/// that is blank. `stream` sets `stream` when the request doesn't.
pub fn convert_openai_request_to_interactions(
    model_name: &str,
    root: &Value,
    stream: bool,
) -> Value {
    let model = first_non_empty(&[model_name, &text_at(root, "model")]);
    let mut out = Map::new();
    out.insert("model".into(), model.into());
    out.insert("input".into(), Value::Array(Vec::new()));
    // `openAIRequestStreamValue`.
    if let Some(value) = root.get("stream") {
        out.insert("stream".into(), bool_of(value).into());
    } else if stream {
        out.insert("stream".into(), true.into());
    }
    let previous = first_non_empty(&[
        &text_at(root, "previous_response_id"),
        &text_at(root, "previous_interaction_id"),
    ]);
    if !previous.is_empty() {
        out.insert("previous_interaction_id".into(), previous.into());
    }
    let environment = first_non_empty(&[
        &text_at(root, "environment_id"),
        &text_at(root, "environment.id"),
    ]);
    if !environment.is_empty() {
        out.insert("environment_id".into(), environment.into());
    }
    if let Some(agent_config) = root.get("agent_config") {
        out.insert("agent_config".into(), agent_config.clone());
    }
    let mut out = Value::Object(out);
    append_messages(&mut out, root.get("messages"));
    copy_generation_config(&mut out, root);
    append_tools(&mut out, root.get("tools"));
    out
}

/// `appendOpenAIMessagesToInteractions`: the system text and the `input`
/// steps of the request's `messages`.
fn append_messages(out: &mut Value, messages: Option<&Value>) {
    let Some(Value::Array(messages)) = messages else {
        return;
    };
    let mut steps = Vec::new();
    let mut system = String::new();
    // The name of each tool call, by its ID, for the results that name none.
    let mut tool_names = HashMap::new();
    for message in messages {
        let role = go::to_lower(text_at(message, "role").trim());
        if role == "system" || role == "developer" {
            let text = content_text(message.get("content"));
            if !text.is_empty() {
                if !system.is_empty() {
                    system.push('\n');
                }
                system.push_str(&text);
            }
        } else {
            append_message(&mut steps, message, &role, &mut tool_names);
        }
    }
    if !system.is_empty() {
        set_path(out, "system_instruction", system.into());
    }
    set_path(out, "input", Value::Array(steps));
}

/// `appendOpenAIMessageToInteractions`: the steps of a message that isn't a
/// system message.
fn append_message(
    steps: &mut Vec<Value>,
    message: &Value,
    role: &str,
    tool_names: &mut HashMap<String, String>,
) {
    match role {
        "assistant" => {
            if let Some(reasoning) = message.get("reasoning_content") {
                steps.extend(
                    reasoning_texts(reasoning)
                        .into_iter()
                        .map(|text| text_step("thought", text)),
                );
            }
            steps.extend(content_step("model_output", message.get("content")));
            if let Some(Value::Array(tool_calls)) = message.get("tool_calls") {
                for tool_call in tool_calls {
                    let id = text_at(tool_call, "id");
                    let name = text_at(tool_call, "function.name");
                    if !id.is_empty() && !name.is_empty() {
                        tool_names.insert(id.into_owned(), name.into_owned());
                    }
                    steps.extend(tool_call_step(tool_call));
                }
            }
        }
        "tool" | "function" => steps.push(tool_result(message, tool_names)),
        _ => steps.extend(content_step("user_input", message.get("content"))),
    }
}

/// `openAIChatContentStep`: a step of `step_type` holding a message's
/// content, if any part of it converts.
fn content_step(step_type: &str, content: Option<&Value>) -> Option<Value> {
    let parts: Vec<Value> = match content {
        Some(Value::String(text)) if text.is_empty() => return None,
        Some(Value::String(text)) => {
            vec![object([
                ("type", "text".into()),
                ("text", text.as_str().into()),
            ])]
        }
        Some(Value::Array(parts)) => parts.iter().filter_map(content_part).collect(),
        Some(part @ Value::Object(_)) => content_part(part).into_iter().collect(),
        _ => Vec::new(),
    };
    if parts.is_empty() {
        return None;
    }
    Some(object([
        ("type", step_type.into()),
        ("content", Value::Array(parts)),
    ]))
}

/// `openAIChatContentPartToInteractions`: a Chat Completions content part
/// as an Interactions one: text, an image, audio or a document. `None` for a
/// part of another type, audio without data, or a document without a file.
fn content_part(part: &Value) -> Option<Value> {
    let mut part_type = go::to_lower(text_at(part, "type").trim());
    if part_type.is_empty() && part.get("text").is_some() {
        part_type = "text".to_owned();
    }
    match part_type.as_str() {
        "text" | "input_text" | "output_text" => Some(object([
            ("type", "text".into()),
            ("text", text_at(part, "text").into()),
        ])),
        "image_url" | "input_image" | "image" => Some(image_part(part)),
        "input_audio" | "audio" => {
            let data =
                first_non_empty(&[&text_at(part, "input_audio.data"), &text_at(part, "data")]);
            if data.is_empty() {
                return None;
            }
            let mut out = Map::new();
            out.insert("type".into(), "audio".into());
            out.insert("data".into(), data.into());
            let format = first_non_empty(&[
                &text_at(part, "input_audio.format"),
                &text_at(part, "format"),
            ]);
            if !format.is_empty() {
                out.insert("mime_type".into(), input_audio_mime_type(&format).into());
            }
            Some(Value::Object(out))
        }
        "file" | "input_file" | "document" => document_part(part),
        _ => None,
    }
}

/// `openAIChatImagePartToInteractions`: an image from a `data:` URL, from
/// base64 `data`, or else by its URL.
fn image_part(part: &Value) -> Value {
    let mut out = Map::new();
    out.insert("type".into(), "image".into());
    let image_url = first_non_empty(&[
        &text_at(part, "image_url.url"),
        &text_at(part, "image_url"),
        &text_at(part, "url"),
    ]);
    if let Some((mime_type, data)) = parse_data_url(&image_url) {
        out.insert("mime_type".into(), mime_type.into());
        out.insert("data".into(), data.into());
        return Value::Object(out);
    }
    let data = text_at(part, "data");
    if !data.is_empty() {
        out.insert("data".into(), data.into());
        let mime_type = text_at(part, "mime_type");
        if !mime_type.is_empty() {
            out.insert("mime_type".into(), mime_type.into());
        }
        return Value::Object(out);
    }
    if !image_url.is_empty() {
        out.insert("image_url".into(), image_url.into());
    }
    Value::Object(out)
}

/// A file, input file or document part as a `document`, with the file's
/// data, typed by its MIME type or its name, or its URL. `None` if it has
/// neither.
fn document_part(part: &Value) -> Option<Value> {
    let filename = first_non_empty(&[&text_at(part, "file.filename"), &text_at(part, "filename")]);
    let fallback_mime_type = first_non_empty(&[
        &text_at(part, "file.mime_type"),
        &text_at(part, "file.mimeType"),
        &text_at(part, "mime_type"),
        &text_at(part, "mimeType"),
    ]);
    let file_data = first_non_empty(&[
        &text_at(part, "file.file_data"),
        &text_at(part, "file_data"),
        &text_at(part, "data"),
    ]);
    let file_url = first_non_empty(&[
        &text_at(part, "file.file_url"),
        &text_at(part, "file_url"),
        &text_at(part, "url"),
    ]);
    let mut out = Map::new();
    out.insert("type".into(), "document".into());
    if !filename.is_empty() {
        out.insert("filename".into(), filename.as_str().into());
    }
    let mut has_content = false;
    if let Some(file) = normalize_openai_file_data(&filename, &fallback_mime_type, &file_data) {
        out.insert("mime_type".into(), file.mime_type.into());
        out.insert("data".into(), file.data.into());
        has_content = true;
    }
    if !file_url.is_empty() {
        out.insert("file_url".into(), file_url.into());
        has_content = true;
    }
    has_content.then_some(Value::Object(out))
}

/// `openAIChatParseDataURL`: the MIME type and data of a base64 `data:` URL.
fn parse_data_url(value: &str) -> Option<(&str, &str)> {
    let (meta, data) = value.strip_prefix("data:")?.split_once(',')?;
    let (mime_type, encoding) = meta.split_once(';').unwrap_or((meta, ""));
    if !go::equal_fold(encoding, "base64") || mime_type.trim().is_empty() || data.is_empty() {
        return None;
    }
    Some((mime_type, data))
}

/// `openAIInputAudioMIMEType`: the MIME type of an `input_audio` format.
fn input_audio_mime_type(format: &str) -> &'static str {
    match go::to_lower(format.trim()).as_str() {
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "opus" => "audio/opus",
        "pcm16" => "audio/pcm",
        _ => "audio/mpeg",
    }
}

/// `openAIToolResultToInteractions`: a tool message as a `function_result`,
/// its content kept as JSON unless it is a string.
fn tool_result(message: &Value, tool_names: &HashMap<String, String>) -> Value {
    let mut out = Map::new();
    out.insert("type".into(), "function_result".into());
    out.insert("result".into(), "".into());
    let call_id = first_non_empty(&[&text_at(message, "tool_call_id"), &text_at(message, "id")]);
    let mut name = text_at(message, "name").into_owned();
    if name.is_empty() && !call_id.is_empty() {
        name = tool_names.get(&call_id).cloned().unwrap_or_default();
    }
    if !call_id.is_empty() {
        out.insert("call_id".into(), call_id.into());
    }
    if !name.is_empty() {
        out.insert("name".into(), name.into());
    }
    if let Some(content) = message.get("content") {
        out.insert("result".into(), content.clone());
    }
    Value::Object(out)
}

/// `openAIChatContentText`: the text of a system message's content.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(content @ Value::Object(_)) => text_at(content, "text").into_owned(),
        Some(Value::Array(parts)) => parts.iter().map(|part| text_at(part, "text")).collect(),
        _ => String::new(),
    }
}

/// `copyOpenAIChatGenerationConfigToInteractions`: the sampling settings,
/// tool choice and reasoning effort into `generation_config`, then the
/// response format, modalities and service tier.
fn copy_generation_config(out: &mut Value, root: &Value) {
    let raw_fields = [
        (
            "generation_config.max_output_tokens",
            first_existing(&[root.get("max_completion_tokens"), root.get("max_tokens")]),
        ),
        ("generation_config.temperature", root.get("temperature")),
        ("generation_config.top_p", root.get("top_p")),
        (
            "generation_config.presence_penalty",
            root.get("presence_penalty"),
        ),
        (
            "generation_config.frequency_penalty",
            root.get("frequency_penalty"),
        ),
        ("generation_config.candidate_count", root.get("n")),
        ("generation_config.stop_sequences", root.get("stop")),
        ("generation_config.tool_choice", root.get("tool_choice")),
    ];
    for (at, value) in raw_fields {
        if let Some(value) = value {
            set_path(out, at, value.clone());
        }
    }
    if let Some(Value::String(effort)) = root.get("reasoning_effort") {
        set_path(
            out,
            "generation_config.thinking_level",
            go::to_lower(effort.trim()).into(),
        );
    }
    if let Some(format) = root.get("response_format") {
        set_path(out, "response_format", format.clone());
    }
    if let Some(modalities) = root.get("modalities") {
        set_path(out, "response_modalities", modalities.clone());
    }
    if let Some(Value::String(tier)) = root.get("service_tier") {
        set_path(out, "service_tier", tier.as_str().into());
    }
}

/// `appendOpenAIChatToolsToInteractions`: the function tools, if any.
fn append_tools(out: &mut Value, tools: Option<&Value>) {
    let Some(Value::Array(tools)) = tools else {
        return;
    };
    let tools: Vec<Value> = tools.iter().filter_map(tool).collect();
    if !tools.is_empty() {
        set_path(out, "tools", Value::Array(tools));
    }
}

/// `openAIChatToolToInteractions`: a function tool, flattened. `None` for a
/// tool of another type, or without a name.
fn tool(tool: &Value) -> Option<Value> {
    let tool_type = go::to_lower(text_at(tool, "type").trim());
    if !tool_type.is_empty() && tool_type != "function" {
        return None;
    }
    let name = first_non_empty(&[&text_at(tool, "function.name"), &text_at(tool, "name")]);
    if name.is_empty() {
        return None;
    }
    let mut out = Map::new();
    out.insert("type".into(), "function".into());
    out.insert("name".into(), name.into());
    let description =
        first_existing(&[path(tool, "function.description"), tool.get("description")]);
    if let Some(description) = description {
        out.insert("description".into(), str_of(Some(description)).into());
    }
    let parameters = first_existing(&[path(tool, "function.parameters"), tool.get("parameters")]);
    if let Some(parameters) = parameters {
        out.insert("parameters".into(), parameters.clone());
    }
    Some(Value::Object(out))
}

/// `ConvertOpenAIResponseToInteractions`: translates a Chat Completions chunk
/// stream into Interactions events, one chunk at a time. Make one per
/// response.
pub struct OpenAIToInteractionsStream {
    /// The model the client asked for, which takes precedence over the
    /// chunks' own.
    model: String,
    created: bool,
    status_updated: bool,
    completed: bool,
    done: bool,
    /// The open step's type and, for a `function_call`, its ID; empty when
    /// no step is open.
    current_step_type: String,
    current_step_id: String,
    /// Each tool call's ID and name, by its index, as the chunks gave them.
    tool_call_ids: HashMap<i64, String>,
    tool_call_names: HashMap<i64, String>,
    /// The interaction's ID: the first chunk's, or one made up.
    id: String,
    /// The index the next step gets.
    step_index: i64,
    active_step_index: i64,
    active_step_open: bool,
    /// The last usage a chunk gave, for the completion at `[DONE]`.
    usage: Option<Value>,
}

impl OpenAIToInteractionsStream {
    /// `model` is the model the client asked for.
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            created: false,
            status_updated: false,
            completed: false,
            done: false,
            current_step_type: String::new(),
            current_step_id: String::new(),
            tool_call_ids: HashMap::new(),
            tool_call_names: HashMap::new(),
            id: String::new(),
            step_index: 0,
            active_step_index: 0,
            active_step_open: false,
            usage: None,
        }
    }

    /// Translates one chunk, a `data:` line or its payload, into the
    /// Interactions event frames it gives, written one after another.
    pub fn translate(&mut self, chunk: &[u8]) -> String {
        let mut out = String::new();
        let payload = sse_payload(chunk);
        if payload.is_empty() {
            return out;
        }
        if go::trim_space(&payload) == b"[DONE]" {
            self.step_stop(&mut out);
            if !self.completed {
                self.complete(&mut out, &Value::Null);
            }
            if !self.done {
                push_frame(&mut out, "done", "[DONE]");
                self.done = true;
            }
            return out;
        }
        let root: Value = serde_json::from_slice(&payload).unwrap_or(Value::Null);
        if let Some(usage) = root.get("usage") {
            self.usage = Some(usage.clone());
        }
        let Some(Value::Array(choices)) = root.get("choices") else {
            return out;
        };
        if choices.is_empty() {
            if root.get("usage").is_some() {
                self.step_stop(&mut out);
                self.complete(&mut out, &root);
            }
            return out;
        }
        for choice in choices {
            if let Some(reasoning) = path(choice, "delta.reasoning_content") {
                for text in reasoning_texts(reasoning) {
                    self.ensure_step(&mut out, "thought", &root);
                    self.text_delta(&mut out, text, true);
                }
            }
            if let Some(content) = path(choice, "delta.content") {
                let text = str_of(Some(content));
                if !text.is_empty() {
                    self.ensure_step(&mut out, "model_output", &root);
                    self.text_delta(&mut out, text.into_owned(), false);
                }
            }
            if let Some(Value::Array(tool_calls)) = path(choice, "delta.tool_calls") {
                for tool_call in tool_calls {
                    self.tool_call_delta(&mut out, &root, tool_call);
                }
            }
            if choice.get("finish_reason").is_some() {
                self.step_stop(&mut out);
            }
        }
        out
    }

    /// `appendOpenAIToolCallDelta`: starts the call's step unless it is the
    /// one open, then sends its arguments, if the chunk has some.
    fn tool_call_delta(&mut self, out: &mut String, root: &Value, tool_call: &Value) {
        let index = int_at(tool_call, "index");
        let id = text_at(tool_call, "id");
        if !id.is_empty() {
            self.tool_call_ids.insert(index, id.into_owned());
        }
        let name = text_at(tool_call, "function.name");
        if !name.is_empty() {
            self.tool_call_names.insert(index, name.into_owned());
        }
        let step_id = first_non_empty(&[
            self.tool_call_ids.get(&index).map_or("", String::as_str),
            &format!("call_{index}"),
        ]);
        if self.current_step_type != "function_call" || self.current_step_id != step_id {
            self.step_stop(out);
            let step_name = self
                .tool_call_names
                .get(&index)
                .cloned()
                .unwrap_or_default();
            self.create(out, root);
            self.step_start(out, "function_call", Some((step_id, step_name)));
        }
        if let Some(arguments) = path(tool_call, "function.arguments") {
            let arguments = str_of(Some(arguments));
            if !arguments.is_empty() {
                let payload = self.delta_event(object([
                    ("arguments", arguments.into()),
                    ("type", "arguments_delta".into()),
                ]));
                push_event(out, "step.delta", &payload);
            }
        }
    }

    /// `appendInteractionsCreated`: `interaction.created`, then
    /// `interaction.status_update`, once.
    fn create(&mut self, out: &mut String, root: &Value) {
        if self.created {
            return;
        }
        self.id = first_non_empty(&[
            &text_at(root, "id"),
            &self.id,
            &format!("interaction_{}", unix_nanos()),
        ]);
        let created = object([
            (
                "interaction",
                object([
                    ("id", self.id.as_str().into()),
                    ("status", "in_progress".into()),
                    ("object", "interaction".into()),
                    ("model", self.response_model(root).into()),
                ]),
            ),
            ("event_type", "interaction.created".into()),
        ]);
        push_event(out, "interaction.created", &created);
        self.created = true;
        if !self.status_updated {
            let status = object([
                ("interaction_id", self.id.as_str().into()),
                ("status", "in_progress".into()),
                ("event_type", "interaction.status_update".into()),
            ]);
            push_event(out, "interaction.status_update", &status);
            self.status_updated = true;
        }
    }

    /// `ensureInteractionsStep`: opens a step of `step_type` unless one is
    /// open, closing any other.
    fn ensure_step(&mut self, out: &mut String, step_type: &str, root: &Value) {
        self.create(out, root);
        if self.active_step_open && self.current_step_type == step_type {
            return;
        }
        self.step_stop(out);
        self.step_start(out, step_type, None);
    }

    /// `appendInteractionsStepStart`: opens the next step; `call` is a
    /// `function_call`'s ID and name.
    fn step_start(&mut self, out: &mut String, step_type: &str, call: Option<(String, String)>) {
        let index = self.step_index;
        self.step_index += 1;
        self.active_step_index = index;
        self.current_step_type = step_type.to_owned();
        self.active_step_open = true;
        let mut step = Map::new();
        step.insert("type".into(), step_type.into());
        if let Some((id, name)) = call {
            if !id.is_empty() {
                step.insert("id".into(), id.as_str().into());
            }
            step.insert("name".into(), name.into());
            step.insert("arguments".into(), Value::Object(Map::new()));
            self.current_step_id = id;
        } else {
            self.current_step_id.clear();
        }
        let payload = object([
            ("index", index.into()),
            ("step", Value::Object(step)),
            ("event_type", "step.start".into()),
        ]);
        push_event(out, "step.start", &payload);
    }

    /// `appendInteractionsTextDelta`: text, or a thought's text, for the
    /// open step.
    fn text_delta(&self, out: &mut String, text: String, thought: bool) {
        let delta = if thought {
            object([
                (
                    "content",
                    object([("text", text.into()), ("type", "text".into())]),
                ),
                ("type", "thought_summary".into()),
            ])
        } else {
            object([("text", text.into()), ("type", "text".into())])
        };
        push_event(out, "step.delta", &self.delta_event(delta));
    }

    /// A `step.delta` event of the open step.
    fn delta_event(&self, delta: Value) -> Value {
        object([
            ("index", self.active_step_index.into()),
            ("delta", delta),
            ("event_type", "step.delta".into()),
        ])
    }

    /// `appendInteractionsStepStop`: closes the open step, if any.
    fn step_stop(&mut self, out: &mut String) {
        if !self.active_step_open {
            return;
        }
        let payload = object([
            ("index", self.active_step_index.into()),
            ("event_type", "step.stop".into()),
        ]);
        push_event(out, "step.stop", &payload);
        self.active_step_open = false;
        self.current_step_type.clear();
        self.current_step_id.clear();
    }

    /// `appendInteractionsCompleted`: `interaction.completed`, once, with the
    /// usage of `root` or else the last chunk's.
    fn complete(&mut self, out: &mut String, root: &Value) {
        if self.completed {
            return;
        }
        if !self.created {
            self.create(out, root);
        }
        let now = create_time(unix_seconds());
        let mut payload = object([
            (
                "interaction",
                object([
                    ("id", self.id.as_str().into()),
                    ("status", "completed".into()),
                    ("usage", Value::Object(Map::new())),
                    ("created", now.as_str().into()),
                    ("updated", now.into()),
                    ("service_tier", "standard".into()),
                    ("object", "interaction".into()),
                    ("model", self.response_model(root).into()),
                ]),
            ),
            ("event_type", "interaction.completed".into()),
        ]);
        let usage = root.get("usage").or(self.usage.as_ref());
        interactions_usage_from_chat(&mut payload, "interaction.usage", usage);
        push_event(out, "interaction.completed", &payload);
        self.completed = true;
    }

    /// The model the client asked for, or else the chunk's.
    fn response_model(&self, root: &Value) -> String {
        first_non_empty(&[&self.model, &text_at(root, "model")])
    }
}

/// `ConvertOpenAIResponseToInteractionsNonStream`: a Chat Completions
/// response as an Interactions one, for `model_name` or else the response's
/// own model.
pub fn convert_openai_response_to_interactions_non_stream(model_name: &str, root: &Value) -> Value {
    let mut out = Map::new();
    let id = first_non_empty(&[
        &text_at(root, "id"),
        &format!("interaction_{}", unix_nanos()),
    ]);
    out.insert("id".into(), id.into());
    out.insert("status".into(), "completed".into());
    out.insert("object".into(), "interaction".into());
    let model = first_non_empty(&[model_name, &text_at(root, "model")]);
    out.insert("model".into(), model.into());
    out.insert("steps".into(), Value::Array(Vec::new()));
    let mut steps = Vec::new();
    for choice in each(root.get("choices")) {
        if let Some(reasoning) = path(choice, "message.reasoning_content") {
            steps.extend(
                reasoning_texts(reasoning)
                    .into_iter()
                    .map(|text| text_step("thought", text)),
            );
        }
        if let Some(content) = path(choice, "message.content") {
            let text = str_of(Some(content));
            if !text.is_empty() {
                steps.push(text_step("model_output", text.into_owned()));
            }
        }
        if let Some(Value::Array(tool_calls)) = path(choice, "message.tool_calls") {
            steps.extend(tool_calls.iter().filter_map(tool_call_step));
        }
        if let Some(finish_reason) = choice.get("finish_reason") {
            out.insert("finish_reason".into(), str_of(Some(finish_reason)).into());
        }
    }
    if !steps.is_empty() {
        out.insert("steps".into(), Value::Array(steps));
    }
    let mut out = Value::Object(out);
    interactions_usage_from_chat(&mut out, "usage", root.get("usage"));
    out
}

#[cfg(test)]
mod tests;
