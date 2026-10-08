// Ported from CLIProxyAPI internal/translator/openai/openai/responses/openai_openai-responses_request.go
// (v8.0.15, MIT), with v8.0.20's user turn refusal and file and audio parts
// (responsesInputFileToChatPart, responsesInputAudioToChatPart) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! OpenAI Responses request → OpenAI Chat Completions request.
//!
//! `instructions` becomes a system message and `input` becomes messages.
//! Consecutive function and custom tool calls become one assistant message
//! with `tool_calls`; a call's output becomes a `tool` message if it answers a
//! call still waiting for one, and otherwise a user message. Tool results are
//! then moved to right after the assistant message that called them, when
//! that can be done safely. Reasoning items become `reasoning_content` on the
//! assistant message that follows them. Tools are merged from `tools` and
//! `additional_tools` input items, with namespace children flattened and names
//! cut to 64 bytes (see [`super::tools`]). The client's local shell becomes a
//! function, and its calls and their outputs in `input` become calls to that
//! function and their outputs (see [`super::shell_tool`]).
//!
//! An `input_file` with a file id or its bytes becomes a `file` part, and an
//! `input_audio` with its bytes an `input_audio` part. A request is refused
//! when a user message is left with nothing to send because its file or audio
//! can't be sent: a file given only by URL, which Chat Completions has no
//! field for, or audio without bytes. Text beside the attachment keeps the
//! message; empty text doesn't. A developer message is sent as a user
//! message, but it isn't the user's turn and is never refused.
//!
//! Deviations from upstream:
//! - Tool parameter schemas are passed on as the client wrote them. Upstream
//!   reads each tool into a Go map and writes it back, which sorts the
//!   schema's keys and rewrites its numbers as `float64`: `1.50` becomes
//!   `1.5`, and integers beyond 2^53 lose precision. The rest of each tool and
//!   tool call has its keys sorted, as upstream writes them.
//! - Where upstream copies the client's raw JSON into a string, we write
//!   compact JSON. The values are the same JSON. This applies to a
//!   non-string `instructions`, message `content` or `text`, `role`, image
//!   URL, `input_file` `file_id`, `file_data` or `filename`, `input_audio`
//!   `data` or `format`, `reasoning_content`, reasoning summary `text` or function call
//!   `arguments`; a custom tool call `input` that isn't a string, inside the
//!   call's `{"input": ...}` arguments; a tool output that isn't a string, or
//!   a part of one that isn't text or an image; a `shell_call`'s `action`
//!   and a whole `shell_call_output` item; and tool names, descriptions and
//!   namespaces.
//! - An empty `reasoning` object counts as no reasoning however it is
//!   written. Upstream compares its text with `{}`, so `{ }` turns reasoning
//!   on there, and tool call turns without reasoning get the
//!   `[reasoning unavailable]` placeholder.
//! - When an object repeats a key, the last value counts; gjson reads the
//!   first.
//! - A tool output string holding JSON that `serde_json` can't read, nested
//!   over 128 levels or with an unpaired surrogate escape, is passed on as
//!   text. gjson reads it, and upstream sends its image parts as images.
//! - A number beyond `f64`'s range in a `text.format` `name`, `description`
//!   or `strict` is kept as written. Go writes it as `+Inf`, or fails to
//!   write the request at all when it's inside an object or array; the same
//!   happens upstream for such a number in a tool's parameter schema.
//! - Names cut to 64 bytes start at a character boundary (see
//!   [`super::tools::cap`]).

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::mem;

use serde_json::{Map, Value, json};

use super::shell_tool;
use super::tool_index::ToolIndex;
use super::tools::tool_output_text;
use crate::common::openai_tools::align_openai_tool_call_messages;
use crate::common::parts::UserTurnDrops;
use crate::common::responses::{extract_responses_call_id, normalize_responses_tool_call_outputs};
use crate::go;
use crate::json::{bool_of, go_value, object, path, raw, set_path, str_of};
use crate::registry::UnsupportedPartError;

/// What upstream writes for reasoning it knows happened but can't show.
const REASONING_UNAVAILABLE: &str = "[reasoning unavailable]";

/// `ConvertOpenAIResponsesRequestToOpenAIChatCompletions`: converts a
/// Responses request for an OpenAI Chat Completions upstream. The refusal
/// names the first user message left with nothing to send.
pub fn convert_openai_responses_request_to_openai_chat_completions(
    model_name: &str,
    request: &Value,
    stream: bool,
) -> (Value, Option<UnsupportedPartError>) {
    let mut out = Map::new();
    out.insert("model".into(), model_name.into());
    out.insert("messages".into(), json!([]));
    out.insert("stream".into(), stream.into());
    let tool_index = ToolIndex::new(request);

    if let Some(format) = path(request, "text.format")
        && let Some(response_format) = convert_text_format(format)
    {
        out.insert("response_format".into(), response_format);
    }
    if let Some(max_tokens) = request.get("max_output_tokens") {
        out.insert("max_tokens".into(), max_tokens.clone());
    }

    let mut history = History::new(request, &tool_index);
    if let Some(instructions) = request.get("instructions") {
        history.messages.push(object([
            ("role", "system".into()),
            ("content", str_of(Some(instructions)).into()),
        ]));
    }
    match request.get("input") {
        Some(Value::Array(items)) => history.convert(&shell_tool::history(items, &tool_index)),
        Some(Value::String(input)) => history.messages.push(object([
            ("role", "user".into()),
            ("content", input.as_str().into()),
        ])),
        _ => {}
    }
    let err = history.drops.err();
    if !history.messages.is_empty() {
        let messages = align_openai_tool_call_messages(
            history.messages,
            history.duplicate_output_ids.iter().map(String::as_str),
        );
        out.insert("messages".into(), Value::Array(messages));
    }

    let tools = tool_index.chat_tools();
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
        if let Some(parallel_tool_calls) = request.get("parallel_tool_calls") {
            out.insert(
                "parallel_tool_calls".into(),
                bool_of(parallel_tool_calls).into(),
            );
        }
        if let Some(tool_choice) = request.get("tool_choice") {
            out.insert(
                "tool_choice".into(),
                convert_tool_choice(tool_choice, &tool_index),
            );
        }
    }

    if let Some(effort) = path(request, "reasoning.effort") {
        let effort = go::to_lower(str_of(Some(effort)).trim());
        if !effort.is_empty() {
            out.insert("reasoning_effort".into(), effort.into());
        }
    }

    (Value::Object(out), err)
}

/// The messages built from `input`, and what converting it keeps track of.
struct History<'i> {
    tool_index: &'i ToolIndex<'i>,
    messages: Vec<Value>,
    /// Whether the conversation reasons, so a tool call turn without
    /// reasoning gets a placeholder.
    has_reasoning: bool,
    /// Tool calls not yet written, each in its Chat Completions form.
    pending_tool_calls: Vec<Value>,
    pending_tool_call_ids: Vec<String>,
    /// Reasoning for the next assistant message.
    pending_reasoning: String,
    /// The last reasoning seen in this turn, for tool calls that have none.
    latest_reasoning: String,
    /// IDs of written tool calls whose output hasn't come yet.
    awaiting_outputs: HashSet<String>,
    output_counts: HashMap<String, usize>,
    /// Call IDs answered more than once.
    duplicate_output_ids: BTreeSet<String>,
    /// The assistant message pending tool calls can join, if it's the last.
    mergeable_assistant: Option<usize>,
    /// The user messages left with nothing to send.
    drops: UserTurnDrops,
}

impl<'i> History<'i> {
    fn new(request: &Value, tool_index: &'i ToolIndex<'i>) -> Self {
        Self {
            tool_index,
            messages: Vec::new(),
            has_reasoning: requests_reasoning(request),
            pending_tool_calls: Vec::new(),
            pending_tool_call_ids: Vec::new(),
            pending_reasoning: String::new(),
            latest_reasoning: String::new(),
            awaiting_outputs: HashSet::new(),
            output_counts: HashMap::new(),
            duplicate_output_ids: BTreeSet::new(),
            mergeable_assistant: None,
            drops: UserTurnDrops::default(),
        }
    }

    fn convert(&mut self, raw_items: &[Value]) {
        let mut explicit_output_counts: HashMap<String, usize> = HashMap::new();
        let mut missing_id_outputs = 0;
        for item in raw_items.iter().filter(|item| is_tool_output(item)) {
            let id = extract_responses_call_id(item);
            if id.is_empty() {
                missing_id_outputs += 1;
            } else {
                *explicit_output_counts.entry(id).or_default() += 1;
            }
        }
        let unclaimed_calls: HashSet<String> = raw_items
            .iter()
            .filter(|item| is_tool_call(item))
            .map(extract_responses_call_id)
            .filter(|id| !id.is_empty() && !explicit_output_counts.contains_key(id))
            .collect();

        let mut items = normalize_responses_tool_call_outputs(raw_items);
        // With more than one way to pair outputs without IDs, none is guessed.
        if missing_id_outputs > 1 || (missing_id_outputs > 0 && unclaimed_calls.len() > 1) {
            for (item, raw_item) in items.iter_mut().zip(raw_items) {
                if is_tool_output(item) && extract_responses_call_id(raw_item).is_empty() {
                    let mut unpaired = (**item).clone();
                    if let Some(fields) = unpaired.as_object_mut() {
                        for key in ["call_id", "tool_call_id", "callId"] {
                            fields.shift_remove(key);
                        }
                    }
                    *item = Cow::Owned(unpaired);
                }
            }
        }

        if !self.has_reasoning {
            self.has_reasoning = raw_items.iter().any(|item| {
                str_of(item.get("type")) == "reasoning" || item.get("reasoning_content").is_some()
            });
        }

        for item in &items {
            self.convert_item(item);
        }
        self.flush_tool_calls();
        self.push_pending_reasoning();
    }

    fn convert_item(&mut self, item: &Value) {
        let mut item_type = str_of(item.get("type"));
        if item_type.is_empty() && !str_of(item.get("role")).is_empty() {
            item_type = Cow::Borrowed("message");
        }
        if item_type != "function_call" && item_type != "custom_tool_call" {
            self.flush_tool_calls();
        }

        match &*item_type {
            "message" | "" => self.convert_message(item),
            "reasoning" => {
                let reasoning = collect_reasoning(item);
                self.pending_reasoning = combine_reasoning(&self.pending_reasoning, &reasoning);
                if is_usable_reasoning(&reasoning) {
                    self.latest_reasoning = reasoning;
                }
            }
            "function_call" => {
                self.take_call_reasoning(item);
                let id = extract_responses_call_id(item);
                let name = match item.get("name") {
                    Some(name) => {
                        let name = str_of(Some(name));
                        let namespace = str_of(item.get("namespace"));
                        match namespace.trim() {
                            "" => self.tool_index.canonical_name(&name),
                            namespace => self.tool_index.namespace_name(namespace, &name),
                        }
                    }
                    None => String::new(),
                };
                let arguments = str_of(item.get("arguments"));
                self.push_tool_call(id, name, arguments.into_owned());
            }
            "custom_tool_call" => {
                self.take_call_reasoning(item);
                let id = extract_responses_call_id(item);
                let name = str_of(item.get("name"));
                let namespace = str_of(item.get("namespace"));
                let name = if namespace.is_empty() {
                    self.tool_index.canonical_name(&name)
                } else {
                    self.tool_index.namespace_name(&namespace, &name)
                };
                // The freeform input, wrapped as a converted custom tool takes it.
                let arguments =
                    format!("{{\"input\":{}}}", sjson_string(&str_of(item.get("input"))));
                self.push_tool_call(id, name, arguments);
            }
            "function_call_output" => self.convert_tool_output(item, function_output_content),
            "custom_tool_call_output" => self.convert_tool_output(item, custom_output_content),
            _ => self.mergeable_assistant = None,
        }
    }

    fn convert_message(&mut self, item: &Value) {
        let mut role = str_of(item.get("role")).into_owned();
        // Only the user's own turn can be refused: a developer message is
        // sent as user text, but it isn't the user's turn.
        let user_turn = role == "user";
        if role == "developer" {
            role = "user".into();
        }
        let assistant = role == "assistant";
        self.mergeable_assistant = None;
        if !assistant {
            self.push_pending_reasoning();
            self.latest_reasoning.clear();
        }

        let content = match item.get("content") {
            Some(Value::Array(parts)) => Value::Array(self.message_parts(parts, user_turn)),
            Some(Value::String(text)) => text.as_str().into(),
            _ => json!([]),
        };
        let mut message = object([("role", role.into()), ("content", content)]);
        if assistant {
            let reasoning = combine_reasoning(
                &mem::take(&mut self.pending_reasoning),
                &str_of(item.get("reasoning_content")),
            );
            if !reasoning.is_empty() {
                set_path(&mut message, "reasoning_content", reasoning.as_str().into());
                if is_usable_reasoning(&reasoning) {
                    self.latest_reasoning = reasoning;
                }
            }
        }
        self.messages.push(message);
        if assistant {
            self.mergeable_assistant = Some(self.messages.len() - 1);
        }
    }

    /// The message's content parts that convert. In a user turn, a file or
    /// audio part that can't be sent is recorded in `drops`, and the turn is
    /// closed with the number of parts it sends; empty text doesn't count.
    fn message_parts(&mut self, parts: &[Value], user_turn: bool) -> Vec<Value> {
        let mut converted = Vec::new();
        let mut sendable = 0;
        for part in parts {
            if let Some(part) = message_part(part) {
                if str_of(part.get("type")) != "text" || !str_of(part.get("text")).is_empty() {
                    sendable += 1;
                }
                converted.push(part);
                continue;
            }
            let part_type = str_of(part.get("type"));
            if user_turn && matches!(part_type.as_ref(), "input_file" | "input_audio") {
                self.drops.drop_part(&part_type);
            }
        }
        if user_turn {
            self.drops.end_turn(sendable);
        }
        converted
    }

    /// A call's `reasoning_content` goes with the calls it's buffered with.
    fn take_call_reasoning(&mut self, item: &Value) {
        let reasoning = str_of(item.get("reasoning_content"));
        self.pending_reasoning = combine_reasoning(&self.pending_reasoning, &reasoning);
        if is_usable_reasoning(&reasoning) {
            self.latest_reasoning = reasoning.into_owned();
        }
    }

    /// Buffers a tool call, as upstream writes it after reading it into a Go
    /// map: with its keys sorted.
    fn push_tool_call(&mut self, id: String, name: String, arguments: String) {
        self.pending_tool_calls.push(object([
            (
                "function",
                object([("arguments", arguments.into()), ("name", name.into())]),
            ),
            ("id", id.as_str().into()),
            ("type", "function".into()),
        ]));
        if !id.is_empty() {
            self.pending_tool_call_ids.push(id);
        }
    }

    fn convert_tool_output(&mut self, item: &Value, content: fn(&Value) -> Value) {
        self.mergeable_assistant = None;
        let call_id = extract_responses_call_id(item);
        if !call_id.is_empty() {
            let count = self.output_counts.entry(call_id.clone()).or_default();
            *count += 1;
            if *count > 1 {
                self.duplicate_output_ids.insert(call_id.clone());
            }
        }

        if !self.awaiting_outputs.remove(&call_id) {
            // An output that answers no waiting call, such as a Codex
            // send_message_to_thread card, becomes user text.
            let content = item.get("output").map_or_else(|| "".into(), content);
            let blank = match &content {
                Value::String(text) => text.trim().is_empty(),
                Value::Array(parts) => parts.is_empty(),
                _ => false,
            };
            if !blank {
                self.messages
                    .push(object([("role", "user".into()), ("content", content)]));
            }
            return;
        }
        let content = item.get("output").map_or_else(|| "".into(), content);
        self.messages.push(object([
            ("role", "tool".into()),
            ("tool_call_id", call_id.into()),
            ("content", content),
        ]));
    }

    /// `fallbackToolReasoning`: reasoning for tool calls that have none.
    fn fallback_reasoning(&self) -> &str {
        if !self.latest_reasoning.is_empty() {
            &self.latest_reasoning
        } else if self.has_reasoning {
            REASONING_UNAVAILABLE
        } else {
            ""
        }
    }

    /// `flushPendingToolCalls`: writes the buffered tool calls, onto the last
    /// message if it's an assistant message without any, or else as a new one.
    fn flush_tool_calls(&mut self) {
        if self.pending_tool_calls.is_empty() {
            return;
        }
        let reasoning = mem::take(&mut self.pending_reasoning);
        let tool_calls = Value::Array(mem::take(&mut self.pending_tool_calls));

        let last = self.messages.len().checked_sub(1);
        let mergeable = self
            .mergeable_assistant
            .filter(|&index| Some(index) == last);
        match mergeable.and_then(|index| self.messages.get(index)) {
            Some(message)
                if str_of(message.get("role")) == "assistant"
                    && message.get("tool_calls").is_none() =>
            {
                let combined =
                    combine_reasoning(&str_of(message.get("reasoning_content")), &reasoning);
                let reasoning = if combined.is_empty() {
                    self.fallback_reasoning().to_owned()
                } else {
                    if is_usable_reasoning(&combined) {
                        self.latest_reasoning = combined.clone();
                    }
                    combined
                };
                if let Some(message) = last.and_then(|index| self.messages.get_mut(index)) {
                    set_path(message, "tool_calls", tool_calls);
                    if !reasoning.is_empty() {
                        set_path(message, "reasoning_content", reasoning.into());
                    }
                }
            }
            _ => {
                let mut message =
                    object([("role", "assistant".into()), ("tool_calls", tool_calls)]);
                let reasoning = if reasoning.is_empty() {
                    self.fallback_reasoning().to_owned()
                } else {
                    if is_usable_reasoning(&reasoning) {
                        self.latest_reasoning = reasoning.clone();
                    }
                    reasoning
                };
                if !reasoning.is_empty() {
                    set_path(&mut message, "reasoning_content", reasoning.into());
                }
                self.messages.push(message);
            }
        }

        for id in mem::take(&mut self.pending_tool_call_ids) {
            let id = id.trim();
            if !id.is_empty() {
                self.awaiting_outputs.insert(id.to_owned());
            }
        }
        self.mergeable_assistant = None;
    }

    /// `appendPendingReasoningMessage`: reasoning no assistant message took
    /// becomes one of its own.
    fn push_pending_reasoning(&mut self) {
        let reasoning = mem::take(&mut self.pending_reasoning);
        if reasoning.is_empty() {
            return;
        }
        if is_usable_reasoning(&reasoning) {
            self.latest_reasoning = reasoning.clone();
        }
        self.messages.push(object([
            ("role", "assistant".into()),
            ("content", "".into()),
            ("reasoning_content", reasoning.into()),
        ]));
    }
}

/// Whether the request turns reasoning on: by `reasoning.effort`, else
/// `reasoning_effort`, else a `reasoning` value other than off.
fn requests_reasoning(request: &Value) -> bool {
    let effort_on = |effort: &Value| {
        let effort = go::to_lower(str_of(Some(effort)).trim());
        !matches!(effort.as_str(), "" | "none" | "0" | "false")
    };
    if let Some(effort) = path(request, "reasoning.effort") {
        effort_on(effort)
    } else if let Some(effort) = request.get("reasoning_effort") {
        effort_on(effort)
    } else if let Some(reasoning) = request.get("reasoning") {
        let reasoning = go::to_lower(str_of(Some(reasoning)).trim());
        !matches!(reasoning.as_str(), "" | "none" | "false" | "{}")
    } else {
        false
    }
}

fn is_tool_call(item: &Value) -> bool {
    matches!(
        &*str_of(item.get("type")),
        "function_call" | "custom_tool_call"
    )
}

fn is_tool_output(item: &Value) -> bool {
    matches!(
        &*str_of(item.get("type")),
        "function_call_output" | "custom_tool_call_output"
    )
}

/// A message content part in Chat Completions form, or `None` for a kind
/// Chat Completions has no part for, a file without an id or bytes, or audio
/// without bytes. A part without a type is text.
fn message_part(part: &Value) -> Option<Value> {
    let part_type = str_of(part.get("type"));
    let part_type = if part_type.is_empty() {
        "input_text"
    } else {
        &part_type
    };
    match part_type {
        "input_text" | "output_text" => Some(object([
            ("type", "text".into()),
            ("text", str_of(part.get("text")).into()),
        ])),
        "input_video" | "video_url" => {
            // A malformed video part is kept, for the upstream to reject,
            // rather than quietly sending the request without the video.
            let mut converted = object([("type", "video_url".into()), ("video_url", json!({}))]);
            match part.get("video_url") {
                Some(video @ Value::Object(_)) => {
                    set_path(&mut converted, "video_url", video.clone());
                }
                Some(url) => {
                    set_path(&mut converted, "video_url.url", url.clone());
                }
                None => {}
            }
            if let Some(processing) = part.get("processing") {
                set_path(&mut converted, "video_url.processing", processing.clone());
            }
            Some(converted)
        }
        "input_image" => {
            let mut image_url = object([("url", str_of(part.get("image_url")).into())]);
            if let Some(detail) = image_detail(part.get("detail"))
                && !detail.is_empty()
            {
                set_path(&mut image_url, "detail", detail.into());
            }
            Some(object([
                ("type", "image_url".into()),
                ("image_url", image_url),
            ]))
        }
        "input_file" => input_file_part(part),
        "input_audio" => input_audio_part(part),
        _ => None,
    }
}

/// `responsesInputFileToChatPart`: an `input_file` as a `file` part, or
/// `None` if it has neither a file id nor its bytes: Chat Completions has no
/// field for a file's URL.
fn input_file_part(part: &Value) -> Option<Value> {
    let file_id = str_of(part.get("file_id"));
    let file_data = str_of(part.get("file_data"));
    if file_id.is_empty() && file_data.is_empty() {
        return None;
    }
    let mut file = Map::new();
    if !file_id.is_empty() {
        file.insert("file_id".into(), file_id.into_owned().into());
    }
    if !file_data.is_empty() {
        file.insert("file_data".into(), file_data.into_owned().into());
    }
    let filename = str_of(part.get("filename"));
    if !filename.is_empty() {
        file.insert("filename".into(), filename.into_owned().into());
    }
    Some(object([
        ("type", "file".into()),
        ("file", Value::Object(file)),
    ]))
}

/// `responsesInputAudioToChatPart`: an `input_audio` as an `input_audio`
/// part, its bytes and format read from `input_audio` or else from the part,
/// or `None` if it has no bytes.
fn input_audio_part(part: &Value) -> Option<Value> {
    let either = |key: &str| {
        let nested = str_of(part.get("input_audio").and_then(|audio| audio.get(key)));
        if nested.is_empty() {
            str_of(part.get(key))
        } else {
            nested
        }
    };
    let data = either("data");
    if data.is_empty() {
        return None;
    }
    let mut audio = Map::new();
    audio.insert("data".into(), data.into_owned().into());
    let format = either("format");
    if !format.is_empty() {
        audio.insert("format".into(), format.into_owned().into());
    }
    Some(object([
        ("type", "input_audio".into()),
        ("input_audio", Value::Object(audio)),
    ]))
}

/// `normalizeChatImageDetail`: an image `detail` Chat Completions accepts, or
/// `""` for none. `None` if it isn't a string.
fn image_detail(detail: Option<&Value>) -> Option<String> {
    let detail = match detail {
        None => return Some(String::new()),
        Some(Value::String(detail)) => go::to_lower(detail.trim()),
        Some(_) => return None,
    };
    Some(match detail.as_str() {
        "auto" | "low" | "high" => detail,
        // Chat Completions has no `original` detail.
        "original" => "high".into(),
        _ => String::new(),
    })
}

/// `setFunctionCallOutputContent`: a function call's output as message
/// content. A list of parts with an image in it, or a string holding one,
/// becomes content parts; anything else becomes text.
fn function_output_content(output: &Value) -> Value {
    let parsed;
    let structured = match output {
        Value::String(text) => {
            if !raw::valid(text) {
                return text.as_str().into();
            }
            match serde_json::from_str::<Value>(text) {
                Ok(value) => {
                    parsed = value;
                    &parsed
                }
                Err(_) => return text.as_str().into(),
            }
        }
        output => output,
    };
    if let Value::Array(items) = structured
        && has_image_part(items)
    {
        return Value::Array(items.iter().map(tool_output_part).collect());
    }
    str_of(Some(output)).into()
}

/// `setCustomToolCallOutputContent`: a custom tool call's output as message
/// content, as [`function_output_content`] if it has an image, else as text.
fn custom_output_content(output: &Value) -> Value {
    let has_image = match output {
        Value::String(text) => {
            raw::valid(text)
                && serde_json::from_str::<Value>(text)
                    .is_ok_and(|value| value.as_array().is_some_and(|items| has_image_part(items)))
        }
        Value::Array(items) => has_image_part(items),
        _ => false,
    };
    if has_image {
        return function_output_content(output);
    }
    tool_output_text(output).into()
}

/// `hasChatToolOutputImagePart`: whether tool output parts include an image
/// and every text and image part is well formed.
fn has_image_part(items: &[Value]) -> bool {
    let mut has_image = false;
    for item in items {
        let Some(Value::String(item_type)) = item.get("type") else {
            continue;
        };
        match item_type.as_str() {
            "text" | "input_text" | "output_text" => {
                if !matches!(item.get("text"), Some(Value::String(_))) {
                    return false;
                }
            }
            "image_url" | "input_image" => {
                if image_fields(item).is_none() {
                    return false;
                }
                has_image = true;
            }
            _ => {}
        }
    }
    has_image
}

/// `chatToolOutputContentPart`: a tool output part as a content part. A part
/// that isn't text or a well-formed image becomes text holding its JSON.
fn tool_output_part(item: &Value) -> Value {
    match &*str_of(item.get("type")) {
        "text" | "input_text" | "output_text" => object([
            ("type", "text".into()),
            ("text", str_of(item.get("text")).into()),
        ]),
        "image_url" | "input_image" => match image_fields(item) {
            Some((url, detail)) => {
                let mut image_url = object([("url", url.into())]);
                if !detail.is_empty() {
                    set_path(&mut image_url, "detail", detail.into());
                }
                object([("type", "image_url".into()), ("image_url", image_url)])
            }
            None => fallback_part(item),
        },
        _ => fallback_part(item),
    }
}

/// `chatToolOutputImageFields`: an image part's trimmed URL and detail, if it
/// has a URL and any detail is a string.
fn image_fields(item: &Value) -> Option<(String, String)> {
    let (url, detail) = match &*str_of(item.get("type")) {
        "image_url" => (path(item, "image_url.url"), path(item, "image_url.detail")),
        "input_image" => (item.get("image_url"), item.get("detail")),
        _ => return None,
    };
    let Some(Value::String(url)) = url else {
        return None;
    };
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    Some((url.to_owned(), image_detail(detail)?))
}

/// `chatToolOutputFallbackPart`: a part as text, its JSON unless it's a
/// string.
fn fallback_part(item: &Value) -> Value {
    let text = match item {
        Value::String(text) => text.clone(),
        item => item.to_string(),
    };
    object([("type", "text".into()), ("text", text.into())])
}

/// `collectOpenAIResponsesReasoningContent`: a reasoning item's summary text,
/// or the placeholder if it has none.
fn collect_reasoning(item: &Value) -> String {
    let mut text = String::new();
    if let Some(Value::Array(summary)) = item.get("summary") {
        for part in summary {
            if str_of(part.get("type")) == "summary_text" {
                text.push_str(&str_of(part.get("text")));
            }
        }
    }
    if text.is_empty() {
        return REASONING_UNAVAILABLE.into();
    }
    text
}

/// `combineOpenAIResponsesReasoning`: two pieces of reasoning as one. A
/// placeholder or a repeat adds nothing.
fn combine_reasoning(existing: &str, incoming: &str) -> String {
    let (existing_trimmed, incoming_trimmed) = (existing.trim(), incoming.trim());
    if existing_trimmed.is_empty() {
        incoming.to_owned()
    } else if incoming_trimmed.is_empty() {
        existing.to_owned()
    } else if existing_trimmed == REASONING_UNAVAILABLE {
        incoming.to_owned()
    } else if incoming_trimmed == REASONING_UNAVAILABLE || existing_trimmed == incoming_trimmed {
        existing.to_owned()
    } else {
        format!("{existing}\n\n{incoming}")
    }
}

/// `isUsableResponsesReasoning`: whether reasoning is more than a
/// placeholder.
fn is_usable_reasoning(reasoning: &str) -> bool {
    let trimmed = reasoning.trim();
    !trimmed.is_empty() && trimmed != REASONING_UNAVAILABLE
}

/// `convertResponsesToolChoiceWithIndex`: a forced function, custom tool or
/// local shell, by its Chat Completions name. Any other choice is passed on
/// as it is.
fn convert_tool_choice(tool_choice: &Value, tool_index: &ToolIndex<'_>) -> Value {
    if !tool_choice.is_object() {
        return tool_choice.clone();
    }
    let choice_type = str_of(tool_choice.get("type"));
    if choice_type == "shell" && !tool_index.shell_name().is_empty() {
        return object([
            ("type", "function".into()),
            (
                "function",
                object([("name", tool_index.shell_name().into())]),
            ),
        ]);
    }
    if !matches!(&*choice_type, "function" | "custom") {
        return tool_choice.clone();
    }
    let Some(name) = ["function.name", "custom.name", "name"]
        .into_iter()
        .map(|key| str_of(path(tool_choice, key)))
        .find(|name| !name.is_empty())
    else {
        return tool_choice.clone();
    };
    let namespace = ["namespace", "function.namespace", "custom.namespace"]
        .into_iter()
        .map(|key| str_of(path(tool_choice, key)).trim().to_owned())
        .find(|namespace| !namespace.is_empty());
    let name = match namespace {
        Some(namespace) => tool_index.namespace_name(&namespace, &name),
        None => tool_index.canonical_name(&name),
    };
    object([
        ("type", "function".into()),
        ("function", object([("name", name.into())])),
    ])
}

/// `convertResponsesTextFormatToChatResponseFormat`: `text.format` as a
/// `response_format`, if it's a kind Chat Completions has.
fn convert_text_format(format: &Value) -> Option<Value> {
    let format_type = str_of(format.get("type"));
    match &*format_type {
        "text" | "json_object" => Some(object([("type", format_type.as_ref().into())])),
        "json_schema" => {
            let mut json_schema = Map::new();
            for field in ["name", "description", "strict"] {
                if let Some(value) = format.get(field) {
                    json_schema.insert(field.into(), go_value(value));
                }
            }
            if let Some(schema) = format.get("schema") {
                json_schema.insert("schema".into(), schema.clone());
            }
            Some(object([
                ("type", "json_schema".into()),
                ("json_schema", Value::Object(json_schema)),
            ]))
        }
        _ => None,
    }
}

/// A string as sjson writes it: with Go's `json.Marshal` when it has a
/// control character, a non-ASCII character, `"` or `\`, and otherwise just
/// quoted, so `<`, `>` and `&` stay as they are.
fn sjson_string(text: &str) -> String {
    if text
        .bytes()
        .any(|b| !(b' '..=0x7f).contains(&b) || b == b'"' || b == b'\\')
    {
        go::json_string(text)
    } else {
        format!("\"{text}\"")
    }
}

#[cfg(test)]
mod tests;
