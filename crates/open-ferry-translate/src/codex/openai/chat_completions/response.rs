// Ported from CLIProxyAPI internal/translator/codex/openai/chat-completions/codex_openai_response.go
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Codex events → OpenAI Chat Completions responses.
//!
//! [`CodexToOpenAIChatCompletionsStream`] turns each Codex event into at most
//! one `chat.completion.chunk`, and
//! [`convert_codex_response_to_openai_chat_completions_non_stream`] turns the
//! final event into one `chat.completion`. Tool names the request translator
//! shortened get their original names back. A call to the custom `apply_patch`
//! tool becomes a function call with the arguments `{"input": "<patch>"}`, so
//! clients that only know function tools can still apply patches.
//!
//! Codex's URL citations become the message's `annotations`, each sent once,
//! with its character range counted from the start of the message's text. A
//! stream counts the text sent before the event that carries the citation, so
//! a citation that comes after all of its text is placed past that text, as
//! upstream places it.
//!
//! Deviations from upstream:
//! - A `data:` line that is not valid JSON gives no chunk. gjson reads what it
//!   can from malformed JSON.
//! - Where upstream uses a value's JSON text, we use the same JSON written
//!   compactly. This applies to a non-string value read as text, so also to
//!   the length of a non-string text delta, which places later citations, and
//!   to the `output_index` that names a tool call.
//! - An invalid `cache_write_tokens` count is dropped as upstream drops it, but
//!   without its logged warning.
//! - A token count or citation index too large for `i64`, such as `1e400`,
//!   saturates. Go's result depends on the CPU; amd64 gives the minimum `i64`.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::request::{build_short_name_map, collect_request_tool_names};
use crate::apply_patch;
use crate::go;
use crate::json::{int_of, object, path, str_of};
use crate::responses_tools::{collect_tool_winners, qualify_namespace_tool_name};

/// Stands in for a missing event or item, as an empty gjson result does upstream.
static NONE: Value = Value::Null;

/// A chunk's `finish_reason` and `native_finish_reason`.
type Finish = (&'static str, String);

/// Translates a Codex event stream into Chat Completions chunks, one line at a
/// time. Keep one per response: it tracks the response's tool calls.
pub struct CodexToOpenAIChatCompletionsStream {
    /// The model the client asked for.
    model: String,
    /// The model `response.created` named, which takes precedence.
    response_model: String,
    response_id: String,
    created_at: i64,
    /// The last service tier Codex reported. Every later chunk repeats it.
    service_tier: String,
    tools: OriginalTools,
    /// Tool calls in this response, in order. A call's position is its
    /// `index` in the chunks.
    calls: Vec<ToolCall>,
    /// Every key an event may name a call by: its item ID or output index.
    call_keys: HashMap<String, usize>,
    /// The call announced last, for events that name no call we know.
    current_call: Option<usize>,
    /// The last image sent for each image item, so a repeat isn't sent again.
    image_hashes: HashMap<String, [u8; 32]>,
    /// The keys of the URL citations sent, so a repeat isn't sent again.
    citation_keys: HashSet<String>,
    /// How many characters of text have been sent, where the next citation's
    /// range starts from.
    text_chars: i64,
}

/// Where one tool call's arguments have got to.
#[derive(Default)]
struct ToolCall {
    arguments_emitted: bool,
    /// Whether this is an `apply_patch` call whose raw patch input is sent as
    /// `{"input": "..."}` arguments.
    patch: bool,
    /// Whether `{"input":"` has been sent.
    input_started: bool,
    /// Whether the closing `"}` (or the whole envelope) has been sent.
    input_closed: bool,
    done: bool,
}

impl ToolCall {
    /// What's left of the `{"input": "..."}` envelope for a patch call: `"}`
    /// if the patch was streamed, or the whole envelope around `input` if not.
    fn finish_patch(&mut self, input: &str) -> String {
        if self.input_closed {
            return String::new();
        }
        self.input_closed = true;
        if self.input_started {
            "\"}".into()
        } else {
            apply_patch::wrap_input(input)
        }
    }
}

impl CodexToOpenAIChatCompletionsStream {
    /// `model` is the model the client asked for, and `original_request` its
    /// Chat Completions request.
    pub fn new(model: &str, original_request: &Value) -> Self {
        Self {
            model: model.to_owned(),
            response_model: model.to_owned(),
            response_id: String::new(),
            created_at: 0,
            service_tier: String::new(),
            tools: OriginalTools::new(original_request),
            calls: Vec::new(),
            call_keys: HashMap::new(),
            current_call: None,
            image_hashes: HashMap::new(),
            citation_keys: HashSet::new(),
            text_chars: 0,
        }
    }

    /// Translates one line of the Codex event stream. Returns the chunk to
    /// send, if the line gives one.
    pub fn translate_line(&mut self, line: &[u8]) -> Option<Value> {
        let data = std::str::from_utf8(line.strip_prefix(b"data:")?).ok()?;
        let event: Value = serde_json::from_str(data.trim()).ok()?;
        let tier =
            codex_service_tier(event.get("response")).or_else(|| codex_service_tier(Some(&event)));
        if let Some(tier) = tier {
            self.service_tier = tier.to_owned();
        }

        let kind = str_of(event.get("type"));
        if kind == "response.created" {
            self.response_id = str_of(path(&event, "response.id")).into_owned();
            self.created_at = path(&event, "response.created_at").map_or(0, int_of);
            self.response_model = str_of(path(&event, "response.model")).into_owned();
            return None;
        }
        let (delta, finish) = self.handle(&kind, &event)?;
        Some(self.chunk(&event, delta, finish))
    }

    /// The chunk's `delta` and finish reasons for one event, or `None` if the
    /// event gives no chunk.
    fn handle(
        &mut self,
        kind: &str,
        event: &Value,
    ) -> Option<(Map<String, Value>, Option<Finish>)> {
        let mut delta = Map::new();
        match kind {
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(text) = event.get("delta") {
                    delta.insert("role".into(), "assistant".into());
                    delta.insert("reasoning_content".into(), str_of(Some(text)).into());
                }
            }
            "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
                delta.insert("role".into(), "assistant".into());
                delta.insert("reasoning_content".into(), "\n\n".into());
            }
            "response.output_text.delta" => {
                if let Some(text) = event.get("delta") {
                    let text = str_of(Some(text));
                    self.text_chars = self.text_chars.wrapping_add(char_count(&text));
                    delta.insert("role".into(), "assistant".into());
                    delta.insert("content".into(), text.into());
                }
            }
            "response.output_text.annotation.added"
            | "response.output_text.done"
            | "response.content_part.done" => delta = self.citations(event)?,
            "response.image_generation_call.partial_image" => {
                let url = self.new_image(
                    &str_of(event.get("item_id")),
                    &str_of(event.get("partial_image_b64")),
                    &str_of(event.get("output_format")),
                )?;
                delta = image_delta(url);
            }
            "response.completed" | "response.incomplete" => {
                let finish = if kind == "response.incomplete" {
                    let native = str_of(path(event, "response.incomplete_details.reason"));
                    (incomplete_finish_reason(&native), native.into_owned())
                } else if self.calls.is_empty() {
                    ("stop", "stop".into())
                } else {
                    ("tool_calls", "tool_calls".into())
                };
                return Some((delta, Some(finish)));
            }
            "response.output_item.added" => delta = self.tool_call_added(event)?,
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                delta = self.arguments_delta(event)?;
            }
            "response.function_call_arguments.done" | "response.custom_tool_call_input.done" => {
                delta = self.arguments_done(kind, event)?;
            }
            "response.output_item.done" => delta = self.item_done(event)?,
            _ => return None,
        }
        Some((delta, None))
    }

    fn chunk(&self, event: &Value, delta: Map<String, Value>, finish: Option<Finish>) -> Value {
        let model = match event.get("model") {
            Some(model) => str_of(Some(model)).into_owned(),
            None if !self.response_model.is_empty() => self.response_model.clone(),
            None if !self.model.is_empty() => self.model.clone(),
            None => "model".into(),
        };
        let (finish_reason, native_finish_reason) = match finish {
            Some((reason, native)) => (reason.into(), native.into()),
            None => (Value::Null, Value::Null),
        };
        let choice = object([
            ("index", 0.into()),
            ("delta", Value::Object(delta)),
            ("finish_reason", finish_reason),
            ("native_finish_reason", native_finish_reason),
        ]);

        let mut chunk = Map::new();
        chunk.insert("id".into(), self.response_id.as_str().into());
        chunk.insert("object".into(), "chat.completion.chunk".into());
        chunk.insert("created".into(), self.created_at.into());
        chunk.insert("model".into(), model.into());
        chunk.insert("choices".into(), Value::Array(vec![choice]));
        if !self.service_tier.is_empty() {
            chunk.insert("service_tier".into(), self.service_tier.as_str().into());
        }
        if let Some(usage) = path(event, "response.usage").and_then(chat_usage) {
            chunk.insert("usage".into(), usage);
        }
        Value::Object(chunk)
    }

    /// A `data:` URL for an image Codex generated, or `None` if there is no
    /// image or it is the one last sent for its item.
    fn new_image(&mut self, item_id: &str, b64: &str, format: &str) -> Option<String> {
        if b64.is_empty() {
            return None;
        }
        if !item_id.is_empty() {
            let hash: [u8; 32] = Sha256::digest(b64.as_bytes()).into();
            if self.image_hashes.get(item_id) == Some(&hash) {
                return None;
            }
            self.image_hashes.insert(item_id.to_owned(), hash);
        }
        Some(image_url(format, b64))
    }

    /// A `delta` of the URL citations in `event` that haven't been sent, or
    /// `None` if there are none.
    fn citations(&mut self, event: &Value) -> Option<Map<String, Value>> {
        let annotations = event_annotations(event);
        let citations = url_citations(annotations, self.text_chars, &mut self.citation_keys);
        if citations.is_empty() {
            return None;
        }
        let mut delta = Map::new();
        delta.insert("role".into(), "assistant".into());
        delta.insert("annotations".into(), Value::Array(citations));
        Some(delta)
    }

    fn tool_call_added(&mut self, event: &Value) -> Option<Map<String, Value>> {
        let item = event.get("item")?;
        if !is_tool_call(item) {
            return None;
        }
        let call = ToolCall {
            patch: self.tools.is_patch(item),
            ..ToolCall::default()
        };
        let index = self.add_call(event, item, call);
        let mut delta = Map::new();
        delta.insert("role".into(), "assistant".into());
        let call = self.tools.tool_call_start(index, item, String::new());
        delta.insert("tool_calls".into(), Value::Array(vec![call]));
        Some(delta)
    }

    fn arguments_delta(&mut self, event: &Value) -> Option<Map<String, Value>> {
        let index = self.find_call(event, &NONE)?;
        let call = &mut self.calls[index];
        let mut arguments = str_of(event.get("delta")).into_owned();
        if call.done || arguments.is_empty() {
            return None;
        }
        call.arguments_emitted = true;
        if call.patch {
            arguments = apply_patch::escape_input_fragment(&arguments);
            if !call.input_started {
                arguments.insert_str(0, "{\"input\":\"");
                call.input_started = true;
            }
        }
        Some(arguments_delta(index, arguments))
    }

    /// The full arguments, if no deltas were sent for them.
    fn arguments_done(&mut self, kind: &str, event: &Value) -> Option<Map<String, Value>> {
        let index = self.find_call(event, &NONE)?;
        let call = &mut self.calls[index];
        if call.done || call.input_closed || (call.arguments_emitted && !call.patch) {
            return None;
        }
        call.arguments_emitted = true;
        let field = if kind == "response.custom_tool_call_input.done" {
            "input"
        } else {
            "arguments"
        };
        let mut arguments = str_of(event.get(field)).into_owned();
        if call.patch {
            arguments = call.finish_patch(&arguments);
        }
        (!arguments.is_empty()).then(|| arguments_delta(index, arguments))
    }

    fn item_done(&mut self, event: &Value) -> Option<Map<String, Value>> {
        let item = event.get("item")?;
        if str_of(item.get("type")) == "message" {
            return self.citations(event);
        }
        if str_of(item.get("type")) == "image_generation_call" {
            let url = self.new_image(
                &str_of(item.get("id")),
                &str_of(item.get("result")),
                &str_of(item.get("output_format")),
            )?;
            return Some(image_delta(url));
        }
        if !is_tool_call(item) {
            return None;
        }

        if let Some(index) = self.find_call(event, item) {
            let call = &mut self.calls[index];
            if call.done {
                return None;
            }
            call.done = true;
            if call.arguments_emitted && (!call.patch || call.input_closed) {
                return None;
            }
            // The call was announced but no arguments came, so send only the
            // arguments; the ID and name are already out.
            call.arguments_emitted = true;
            let mut arguments = tool_call_arguments(item);
            if call.patch {
                arguments = call.finish_patch(&arguments);
            }
            return (!arguments.is_empty()).then(|| arguments_delta(index, arguments));
        }

        // Codex skipped `output_item.added`, so the whole call goes out now.
        let mut call = ToolCall {
            arguments_emitted: true,
            done: true,
            patch: self.tools.is_patch(item),
            ..ToolCall::default()
        };
        let mut arguments = tool_call_arguments(item);
        if call.patch {
            arguments = call.finish_patch(&arguments);
        }
        let index = self.add_call(event, item, call);
        let call = self.tools.tool_call_start(index, item, arguments);
        let mut delta = Map::new();
        delta.insert("tool_calls".into(), Value::Array(vec![call]));
        delta.insert("role".into(), "assistant".into());
        Some(delta)
    }

    fn add_call(&mut self, event: &Value, item: &Value, call: ToolCall) -> usize {
        let index = self.calls.len();
        self.calls.push(call);
        for key in call_keys(event, item) {
            self.call_keys.insert(key, index);
        }
        self.current_call = Some(index);
        index
    }

    fn find_call(&self, event: &Value, item: &Value) -> Option<usize> {
        call_keys(event, item)
            .find_map(|key| self.call_keys.get(&key).copied())
            .or(self.current_call)
    }
}

/// The keys an event names a tool call by, in the order they're tried.
fn call_keys(event: &Value, item: &Value) -> impl Iterator<Item = String> {
    let item_ids = [event.get("item_id"), item.get("id")].map(|id| str_of(id).into_owned());
    item_ids
        .into_iter()
        .filter(|id| !id.is_empty())
        .map(|id| format!("item:{id}"))
        .chain(
            event
                .get("output_index")
                .map(|index| format!("output:{index}")),
        )
}

/// Converts Codex's final event into one Chat Completions response. Returns
/// `None` for any event but `response.completed` or `response.incomplete`.
/// `original_request` is the client's Chat Completions request.
pub fn convert_codex_response_to_openai_chat_completions_non_stream(
    original_request: &Value,
    event: &Value,
) -> Option<Value> {
    let kind = str_of(event.get("type"));
    if kind != "response.completed" && kind != "response.incomplete" {
        return None;
    }
    let response = event.get("response").unwrap_or(&NONE);
    let tools = OriginalTools::new(original_request);

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    let mut images = Vec::new();
    let mut annotations = Vec::new();
    let mut annotation_keys = HashSet::new();
    // The characters of message text before the part being read.
    let mut text_chars: i64 = 0;
    if let Some(Value::Array(output)) = response.get("output") {
        for item in output {
            match &*str_of(item.get("type")) {
                "reasoning" => {
                    // The first summary part, then every reasoning text part.
                    if let Some(text) = first_part(item.get("summary"), "summary_text") {
                        reasoning.push_str(&text);
                    }
                    if let Some(Value::Array(parts)) = item.get("content") {
                        for part in parts {
                            if str_of(part.get("type")) == "reasoning_text" {
                                reasoning.push_str(&str_of(part.get("text")));
                            }
                        }
                    }
                }
                "message" => {
                    // Every text part, and the URL citations in each.
                    let Some(Value::Array(parts)) = item.get("content") else {
                        continue;
                    };
                    for part in parts {
                        if str_of(part.get("type")) != "output_text" {
                            continue;
                        }
                        let text = str_of(part.get("text"));
                        content.push_str(&text);
                        let part_annotations = annotation_list(part.get("annotations"));
                        annotations.extend(url_citations(
                            part_annotations,
                            text_chars,
                            &mut annotation_keys,
                        ));
                        text_chars = text_chars.wrapping_add(char_count(&text));
                    }
                }
                "function_call" | "custom_tool_call" => tool_calls.push(tools.tool_call(item)),
                "image_generation_call" => {
                    let b64 = str_of(item.get("result"));
                    if !b64.is_empty() {
                        let url = image_url(&str_of(item.get("output_format")), &b64);
                        images.push(image_part(images.len(), url));
                    }
                }
                _ => {}
            }
        }
    }

    let finish = match response.get("status").map(|status| str_of(Some(status))) {
        Some(status) if status == "completed" => Some(if tool_calls.is_empty() {
            ("stop", "stop".to_owned())
        } else {
            ("tool_calls", "tool_calls".to_owned())
        }),
        Some(status) if status == "incomplete" => {
            let native = str_of(path(response, "incomplete_details.reason"));
            Some((incomplete_finish_reason(&native), native.into_owned()))
        }
        _ => None,
    };
    let (finish_reason, native_finish_reason) = match finish {
        Some((reason, native)) => (reason.into(), native.into()),
        None => (Value::Null, Value::Null),
    };

    let text_or_null = |text: String| {
        if text.is_empty() {
            Value::Null
        } else {
            text.into()
        }
    };
    let mut message = Map::new();
    message.insert("role".into(), "assistant".into());
    message.insert("content".into(), text_or_null(content));
    message.insert("reasoning_content".into(), text_or_null(reasoning));
    let tool_calls = if tool_calls.is_empty() {
        Value::Null
    } else {
        Value::Array(tool_calls)
    };
    message.insert("tool_calls".into(), tool_calls);
    if !annotations.is_empty() {
        message.insert("annotations".into(), Value::Array(annotations));
    }
    if !images.is_empty() {
        message.insert("images".into(), Value::Array(images));
    }
    let choice = object([
        ("index", 0.into()),
        ("message", Value::Object(message)),
        ("finish_reason", finish_reason),
        ("native_finish_reason", native_finish_reason),
    ]);

    let created = response.get("created_at").map_or_else(unix_now, int_of);
    let mut out = Map::new();
    out.insert("id".into(), str_of(response.get("id")).into());
    out.insert("object".into(), "chat.completion".into());
    out.insert("created".into(), created.into());
    let model = response
        .get("model")
        .map_or("model".into(), |model| str_of(Some(model)));
    out.insert("model".into(), model.into());
    out.insert("choices".into(), Value::Array(vec![choice]));
    let tier = codex_service_tier(Some(response)).or_else(|| codex_service_tier(Some(event)));
    if let Some(tier) = tier {
        out.insert("service_tier".into(), tier.into());
    }
    if let Some(usage) = response.get("usage").and_then(chat_usage) {
        out.insert("usage".into(), usage);
    }
    Some(Value::Object(out))
}

/// The tools the client declared, as the responses need them.
struct OriginalTools {
    /// Codex tool name → the name the client used.
    client_names: HashMap<String, String>,
    /// Names that refer to the custom `apply_patch` tool.
    patch_names: HashSet<String>,
}

impl OriginalTools {
    fn new(original_request: &Value) -> Self {
        let client_names = build_short_name_map(&collect_request_tool_names(original_request))
            .into_iter()
            .map(|(client, codex)| (codex, client))
            .collect();

        // A name also declared as a function is that function: a function call
        // can't say it meant the custom tool. gjson reads a `tools` value that
        // isn't a list as a list of that one value.
        let tools = match original_request.get("tools") {
            Some(Value::Array(tools)) => tools.as_slice(),
            None | Some(Value::Null) => &[],
            Some(tool) => std::slice::from_ref(tool),
        };
        let function_names: HashSet<_> = tools
            .iter()
            .filter(|tool| str_of(tool.get("type")) == "function")
            .map(|tool| str_of(path(tool, "function.name")))
            .collect();
        let patch_names = collect_tool_winners(original_request)
            .into_iter()
            .filter(|(name, winner)| {
                apply_patch::is_custom_tool(winner.tool) && !function_names.contains(name.as_str())
            })
            .map(|(name, _)| name)
            .collect();
        Self {
            client_names,
            patch_names,
        }
    }

    fn client_name(&self, name: &str) -> String {
        self.client_names
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_owned())
    }

    /// Whether `item` is a call to the client's custom `apply_patch` tool.
    fn is_patch(&self, item: &Value) -> bool {
        if str_of(item.get("type")) != "custom_tool_call" {
            return false;
        }
        let name =
            qualify_namespace_tool_name(&str_of(item.get("namespace")), &str_of(item.get("name")));
        self.patch_names.contains(&name)
    }

    /// A streamed tool call's first chunk entry, with its ID and name.
    fn tool_call_start(&self, index: usize, item: &Value, arguments: String) -> Value {
        object([
            ("index", index.into()),
            ("id", str_of(item.get("call_id")).into()),
            ("type", "function".into()),
            ("function", self.function(item, arguments)),
        ])
    }

    /// A tool call in a non-streaming response.
    fn tool_call(&self, item: &Value) -> Value {
        let mut arguments = tool_call_arguments(item);
        if self.is_patch(item) {
            arguments = apply_patch::wrap_input(&arguments);
        }
        object([
            ("id", str_of(item.get("call_id")).into()),
            ("type", "function".into()),
            ("function", self.function(item, arguments)),
        ])
    }

    fn function(&self, item: &Value, arguments: String) -> Value {
        let name = self.client_name(&str_of(item.get("name")));
        object([("name", name.into()), ("arguments", arguments.into())])
    }
}

fn is_tool_call(item: &Value) -> bool {
    matches!(
        &*str_of(item.get("type")),
        "function_call" | "custom_tool_call"
    )
}

/// A call's arguments, or a custom tool call's input.
fn tool_call_arguments(item: &Value) -> String {
    let field = if str_of(item.get("type")) == "custom_tool_call" {
        "input"
    } else {
        "arguments"
    };
    str_of(item.get(field)).into_owned()
}

fn arguments_delta(index: usize, arguments: String) -> Map<String, Value> {
    let function = object([("arguments", arguments.into())]);
    let call = object([("index", index.into()), ("function", function)]);
    Map::from_iter([("tool_calls".into(), Value::Array(vec![call]))])
}

/// How many characters `text` has, as Go counts runes.
fn char_count(text: &str) -> i64 {
    i64::try_from(text.chars().count()).unwrap_or(i64::MAX)
}

/// The annotations in an `annotations` value: a list's items, or any other
/// value as the one annotation (`codexAnnotationResults`).
fn annotation_list(value: Option<&Value>) -> &[Value] {
    match value {
        None => &[],
        Some(Value::Array(items)) => items,
        Some(value) => std::slice::from_ref(value),
    }
}

/// The annotations a stream event carries, wherever Codex puts them: as the
/// event's `annotation` or `annotations`, in its content `part`, or in its
/// output `item` and that item's content parts (`codexAnnotationsFromEvent`).
fn event_annotations(event: &Value) -> Vec<&Value> {
    let mut annotations = Vec::new();
    for at in ["annotation", "annotations", "part.annotations"] {
        annotations.extend(annotation_list(path(event, at)));
    }
    if let Some(item) = event.get("item") {
        annotations.extend(annotation_list(item.get("annotations")));
        if let Some(Value::Array(parts)) = item.get("content") {
            for part in parts {
                annotations.extend(annotation_list(part.get("annotations")));
            }
        }
    }
    annotations
}

/// Chat Completions `url_citation`s for the URL citations in `annotations`,
/// their ranges moved `offset` characters on (`buildCodexURLCitations`). A
/// citation is left out if its range is negative or backwards, or if `seen`
/// holds its URL (or, without one, its ID) with the same start, or its ID.
/// The keys of the others are added to `seen`.
fn url_citations<'v>(
    annotations: impl IntoIterator<Item = &'v Value>,
    offset: i64,
    seen: &mut HashSet<String>,
) -> Vec<Value> {
    let mut citations = Vec::new();
    for annotation in annotations {
        if str_of(annotation.get("type")) != "url_citation" {
            continue;
        }
        let index = |key| annotation.get(key).map_or(0, int_of);
        let raw_start = index("start_index");
        // Go's int64 addition wraps.
        let start = raw_start.wrapping_add(offset);
        let end = index("end_index").wrapping_add(offset);
        if start < 0 || end < start {
            continue;
        }

        let url = str_of(annotation.get("url"));
        let id = str_of(annotation.get("id"));
        let source = if url.is_empty() { &id } else { &url };
        let mut keys = vec![format!("{source}\0{raw_start}")];
        if !id.is_empty() {
            keys.push(format!("id\0{id}"));
        }
        if keys.iter().any(|key| seen.contains(key)) {
            continue;
        }
        seen.extend(keys);
        citations.push(object([
            ("type", "url_citation".into()),
            ("url", url.into()),
            ("title", str_of(annotation.get("title")).into()),
            ("start_index", start.into()),
            ("end_index", end.into()),
        ]));
    }
    citations
}

/// The text of the first part of `kind` in a list of content parts.
fn first_part(parts: Option<&Value>, kind: &str) -> Option<String> {
    let Some(Value::Array(parts)) = parts else {
        return None;
    };
    let part = parts.iter().find(|part| str_of(part.get("type")) == kind)?;
    Some(str_of(part.get("text")).into_owned())
}

fn image_delta(url: String) -> Map<String, Value> {
    let mut delta = Map::new();
    delta.insert("images".into(), Value::Array(vec![image_part(0, url)]));
    delta.insert("role".into(), "assistant".into());
    delta
}

fn image_part(index: usize, url: String) -> Value {
    object([
        ("type", "image_url".into()),
        ("image_url", object([("url", url.into())])),
        ("index", index.into()),
    ])
}

fn image_url(format: &str, b64: &str) -> String {
    format!("data:{};base64,{b64}", mime_type(format))
}

/// The MIME type of an image in Codex's `output_format`, which is a MIME type
/// or a file extension. PNG unless it says otherwise.
fn mime_type(format: &str) -> &str {
    if format.contains('/') {
        return format;
    }
    match go::to_lower(format).as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "image/png",
    }
}

fn incomplete_finish_reason(reason: &str) -> &'static str {
    match reason {
        "max_tokens" | "max_output_tokens" => "length",
        "content_filter" => "content_filter",
        _ => "stop",
    }
}

/// The service tier Codex reported in `value`, trimmed, if it is a non-empty
/// string.
fn codex_service_tier(value: Option<&Value>) -> Option<&str> {
    let tier = value?.get("service_tier")?.as_str()?.trim();
    (!tier.is_empty()).then_some(tier)
}

/// Codex `usage` → Chat Completions `usage`, or `None` if it has no counts.
fn chat_usage(usage: &Value) -> Option<Value> {
    let count = |field| path(usage, field).map(|count| Value::from(int_of(count)));
    let mut out = Map::new();
    let fields = [
        ("completion_tokens", "output_tokens"),
        ("total_tokens", "total_tokens"),
        ("prompt_tokens", "input_tokens"),
    ];
    for (key, field) in fields {
        if let Some(count) = count(field) {
            out.insert(key.into(), count);
        }
    }
    let mut prompt_details = Map::new();
    if let Some(count) = count("input_tokens_details.cached_tokens") {
        prompt_details.insert("cached_tokens".into(), count);
    }
    if let Some(count) = cache_write_tokens(usage) {
        prompt_details.insert("cache_write_tokens".into(), count.clone());
        prompt_details.insert("cached_creation_tokens".into(), count);
    }
    if !prompt_details.is_empty() {
        out.insert(
            "prompt_tokens_details".into(),
            Value::Object(prompt_details),
        );
    }
    if let Some(count) = count("output_tokens_details.reasoning_tokens") {
        let details = object([("reasoning_tokens", count)]);
        out.insert("completion_tokens_details".into(), details);
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

/// `input_tokens_details.cache_write_tokens`, if it is written as a whole
/// number. Upstream copies its digits rather than converting them.
fn cache_write_tokens(usage: &Value) -> Option<Value> {
    match path(usage, "input_tokens_details.cache_write_tokens")? {
        Value::Number(count) if count.to_string().bytes().all(|b| b.is_ascii_digit()) => {
            Some(Value::Number(count.clone()))
        }
        _ => None,
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests;
