// Ported from CLIProxyAPI internal/translator/openai/claude/openai_claude_response.go
// (ConvertOpenAIResponseToClaude, ConvertOpenAIResponseToClaudeNonStream, ClaudeTokenCount)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! OpenAI Chat Completions responses → Claude Messages responses.
//!
//! [`OpenAIToClaudeStream`] turns each line of a Chat Completions stream into
//! Claude SSE events, and [`convert_openai_response_to_claude_non_stream`]
//! turns a whole Chat Completions response into one Claude message.
//!
//! Claude content blocks can't interleave, so one tool call streams at a
//! time. Text and reasoning that arrive while a call's block is open are held
//! back and sent as blocks of their own once the calls are finished. A call's
//! arguments are sent in one piece when it finishes.
//!
//! Deviations from upstream:
//! - A `data:` line or a response that is not valid JSON is treated as one
//!   with no fields. gjson reads what it can from malformed JSON.
//! - Where upstream copies JSON into a string, we write the same JSON as
//!   serde_json formats it: an `id`, `model`, content, reasoning text or
//!   tool call arguments that are objects or arrays, read as text.
//! - Tool call arguments that gjson accepts but serde_json can't read (with
//!   a lone UTF-16 surrogate escape, or nested deeper than 128 levels) give
//!   an empty `input` in a whole message, where upstream copies them.

use std::collections::{BTreeMap, HashMap};

use serde_json::{Map, Value};

use crate::common::claude::sanitize_tool_id;
use crate::common::tool_names::{
    ToolNameMap, fix_json, map_tool_name, tool_name_map_from_claude_request,
};
use crate::go;
use crate::json::{int_of, object, path, raw, str_of};

/// Converts a Claude `count_tokens` result into its response body.
pub fn claude_token_count(count: i64) -> Value {
    object([("input_tokens", count.into())])
}

/// Translates a Chat Completions stream into Claude Messages SSE events, one
/// line at a time. Keep one per response: it tracks which content blocks are
/// open.
#[derive(Debug)]
pub struct OpenAIToClaudeStream {
    /// The client's tools by canonical name, to give tool calls back the
    /// names the client declared.
    tool_names: Option<ToolNameMap>,
    /// Whether the client asked for a stream. If it didn't, each line is
    /// read as a whole response.
    streaming: bool,
    message_id: String,
    model: String,
    message_started: bool,
    /// The index of the open text block.
    text_block: Option<i64>,
    /// The index of the open thinking block.
    thinking_block: Option<i64>,
    next_block: i64,
    /// Whether any text arrived, sent or held back.
    content_seen: bool,
    /// Tool calls by their index in the stream.
    tool_calls: BTreeMap<i64, ToolCall>,
    /// The content block index of each started call that hasn't finished.
    tool_blocks: HashMap<i64, i64>,
    /// The stream index of the call whose block is open, or -1. A call with
    /// index -1 is never seen as open, as upstream doesn't.
    open_tool_call: i64,
    /// Whether a `tool_use` block was started.
    saw_tool_call: bool,
    /// Text and reasoning held back while a call's block was open.
    held: Vec<Held>,
    finish_reason: String,
    /// Whether the tool calls have been finished and held content sent.
    blocks_stopped: bool,
    message_delta_sent: bool,
    message_stop_sent: bool,
    usage: Usage,
}

#[derive(Debug, Default)]
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
    started: bool,
}

#[derive(Debug)]
struct Held {
    thinking: bool,
    text: String,
}

#[derive(Clone, Copy, Debug, Default)]
struct Usage {
    input: i64,
    output: i64,
    cached: i64,
    cache_write: i64,
}

impl OpenAIToClaudeStream {
    /// Starts a stream answering `original_request`, the client's Claude
    /// request.
    pub fn new(original_request: &Value) -> Self {
        Self {
            tool_names: tool_name_map_from_claude_request(original_request),
            streaming: !matches!(
                original_request.get("stream"),
                None | Some(Value::Bool(false))
            ),
            message_id: String::new(),
            model: String::new(),
            message_started: false,
            text_block: None,
            thinking_block: None,
            next_block: 0,
            content_seen: false,
            tool_calls: BTreeMap::new(),
            tool_blocks: HashMap::new(),
            open_tool_call: -1,
            saw_tool_call: false,
            held: Vec::new(),
            finish_reason: String::new(),
            blocks_stopped: false,
            message_delta_sent: false,
            message_stop_sent: false,
            usage: Usage::default(),
        }
    }

    /// Translates one line of the Chat Completions stream. Returns zero or
    /// more chunks, each one complete Claude SSE event (`event: …\ndata:
    /// …\n\n`); lines other than `data:` lines produce nothing. If the client
    /// didn't ask for a stream, each `data:` line other than `[DONE]` gives
    /// one Claude message, as JSON.
    pub fn translate_line(&mut self, line: &[u8]) -> Vec<String> {
        let Some(data) = line.strip_prefix(b"data:") else {
            return Vec::new();
        };
        let data = go::trim_space(data);
        if data == b"[DONE]" {
            return self.done();
        }
        let chunk = serde_json::from_slice(data).unwrap_or(Value::Null);
        if !self.streaming {
            return vec![chunk_message(&chunk).to_string()];
        }
        let mut out = Vec::new();
        self.chunk(&chunk, &mut out);
        out
    }

    fn chunk(&mut self, chunk: &Value, out: &mut Vec<String>) {
        if self.message_id.is_empty() {
            self.message_id = str_of(chunk.get("id")).into_owned();
        }
        if self.model.is_empty() {
            self.model = str_of(chunk.get("model")).into_owned();
        }

        let choice = first_choice(chunk);
        // message_start goes out with the first delta, whatever it holds:
        // some providers send a tool call in the first chunk, with no role.
        if let Some(delta) = choice.and_then(|choice| choice.get("delta")) {
            if !self.message_started {
                let message = message_template(&self.message_id, &self.model);
                push_event(
                    out,
                    "message_start",
                    &object([("type", "message_start".into()), ("message", message)]),
                );
                self.message_started = true;
            }
            for text in reasoning_texts(delta) {
                self.reasoning(text, out);
            }
            if let Some(content) = delta.get("content") {
                let text = str_of(Some(content));
                if !text.is_empty() {
                    self.text(&text, out);
                }
            }
            if let Some(Value::Array(calls)) = delta.get("tool_calls") {
                for (position, call) in calls.iter().enumerate() {
                    self.tool_call_delta(position, call, out);
                }
            }
        }

        // The finish reason is recorded here; message_delta waits for usage
        // or [DONE].
        let reason = str_of(choice.and_then(|choice| choice.get("finish_reason")));
        if !reason.is_empty() {
            self.finish_reason = match &*reason {
                "length" | "content_filter" => reason.into_owned(),
                _ if self.saw_tool_call => if self.tool_arguments_valid() {
                    "tool_calls"
                } else {
                    "length"
                }
                .to_owned(),
                "tool_calls" => "stop".to_owned(),
                _ => reason.into_owned(),
            };
            self.finalize(out);
        }

        let usage = chunk.get("usage").filter(|usage| !usage.is_null());
        if let Some(usage) = usage {
            self.usage = extract_usage(usage);
        }
        // A chunk with usage and no choice, after anything was sent, ends the
        // response as a finish reason does.
        let trailing_usage = usage.is_some()
            && choice.is_none()
            && (!self.finish_reason.is_empty()
                || self.saw_tool_call
                || self.text_block.is_some()
                || self.thinking_block.is_some()
                || self.content_seen
                || !self.held.is_empty());
        if !self.message_delta_sent
            && (!self.finish_reason.is_empty() || trailing_usage)
            && usage.is_some()
        {
            self.finalize(out);
            self.message_delta(out);
            self.message_stop(out);
        }
    }

    /// `[DONE]`: finishes whatever is open and ends the message, whether or
    /// not it started.
    fn done(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        self.finalize(&mut out);
        self.message_delta(&mut out);
        self.message_stop(&mut out);
        out
    }

    fn reasoning(&mut self, text: String, out: &mut Vec<String>) {
        if self.open_tool_call != -1 {
            self.hold(true, &text);
            return;
        }
        self.stop_text(out);
        let index = match self.thinking_block {
            Some(index) => index,
            None => {
                let index = self.take_block_index();
                self.thinking_block = Some(index);
                push_block_start(out, index, thinking_block(""));
                index
            }
        };
        push_block_delta(
            out,
            index,
            object([("type", "thinking_delta".into()), ("thinking", text.into())]),
        );
    }

    fn text(&mut self, text: &str, out: &mut Vec<String>) {
        self.content_seen = true;
        if self.open_tool_call != -1 {
            self.hold(false, text);
            return;
        }
        let index = match self.text_block {
            Some(index) => index,
            None => {
                self.stop_thinking(out);
                let index = self.take_block_index();
                self.text_block = Some(index);
                push_block_start(out, index, text_block(""));
                index
            }
        };
        push_block_delta(
            out,
            index,
            object([("type", "text_delta".into()), ("text", text.into())]),
        );
    }

    /// Holds back text or reasoning, adding to the last held chunk if it is
    /// the same kind.
    fn hold(&mut self, thinking: bool, text: &str) {
        match self.held.last_mut() {
            Some(last) if last.thinking == thinking => last.text.push_str(text),
            _ => self.held.push(Held {
                thinking,
                text: text.to_owned(),
            }),
        }
    }

    fn tool_call_delta(&mut self, position: usize, delta: &Value, out: &mut Vec<String>) {
        let index = match delta.get("index") {
            Some(index) => int_of(index),
            None => position as i64,
        };
        let names = self.tool_names.as_ref();
        let call = self.tool_calls.entry(index).or_default();
        // Only a non-empty string replaces the ID.
        if let Some(Value::String(id)) = delta.get("id")
            && !id.is_empty()
        {
            call.id.clone_from(id);
        }
        if let Some(function) = delta.get("function") {
            // The name is only taken until the block starts: some providers
            // repeat it, or send it empty, in later chunks.
            if !call.started
                && let Some(Value::String(name)) = function.get("name")
                && !name.is_empty()
            {
                call.name = map_tool_name(names, name);
            }
            if let Some(arguments) = function.get("arguments") {
                call.arguments.push_str(&str_of(Some(arguments)));
            }
        }
        // Checked on every chunk: some providers send the name and ID in
        // different chunks.
        if !call.started
            && !call.name.is_empty()
            && !call.id.is_empty()
            && !self.blocks_stopped
            && self.open_tool_call == -1
        {
            self.start_tool_use(index, out);
        }
    }

    /// Sends the `tool_use` block's start for call `index`, closing any open
    /// text or thinking block first.
    fn start_tool_use(&mut self, index: i64, out: &mut Vec<String>) {
        self.stop_thinking(out);
        self.stop_text(out);
        let block = self.tool_block_index(index);
        let call = self
            .tool_calls
            .get_mut(&index)
            .expect("only a recorded call starts");
        let content_block = object([
            ("type", "tool_use".into()),
            ("id", sanitize_tool_id(&call.id).into()),
            ("name", call.name.clone().into()),
            ("input", Value::Object(Map::new())),
        ]);
        push_block_start(out, block, content_block);
        call.started = true;
        self.saw_tool_call = true;
        self.open_tool_call = index;
    }

    fn tool_block_index(&mut self, index: i64) -> i64 {
        if let Some(&block) = self.tool_blocks.get(&index) {
            return block;
        }
        let block = self.take_block_index();
        self.tool_blocks.insert(index, block);
        block
    }

    fn take_block_index(&mut self) -> i64 {
        let index = self.next_block;
        self.next_block += 1;
        index
    }

    /// Sends call `index`'s arguments and closes its block. A call that never
    /// started is started first, named `tool_<index>` if it has no name,
    /// unless it has no ID, name or arguments either.
    fn finish_tool_call(&mut self, index: i64, out: &mut Vec<String>) {
        let Some(call) = self.tool_calls.get_mut(&index) else {
            return;
        };
        if !call.started {
            if call.name.is_empty() && call.id.is_empty() && call.arguments.is_empty() {
                return;
            }
            if call.name.is_empty() {
                call.name = format!("tool_{index}");
            }
            self.start_tool_use(index, out);
        }
        let block = self.tool_block_index(index);
        let arguments = &self.tool_calls[&index].arguments;
        if !arguments.is_empty() {
            push_block_delta(
                out,
                block,
                object([
                    ("type", "input_json_delta".into()),
                    ("partial_json", fix_json(arguments).into()),
                ]),
            );
        }
        push_block_stop(out, block);
        self.tool_blocks.remove(&index);
        self.open_tool_call = -1;
    }

    /// Closes the open text and thinking blocks, then, the first time, every
    /// tool call, and sends the content held back.
    fn finalize(&mut self, out: &mut Vec<String>) {
        self.stop_thinking(out);
        self.stop_text(out);
        if self.blocks_stopped {
            return;
        }
        if self.open_tool_call != -1 {
            self.finish_tool_call(self.open_tool_call, out);
        }
        let waiting: Vec<i64> = self
            .tool_calls
            .iter()
            .filter(|(_, call)| !call.started)
            .map(|(&index, _)| index)
            .collect();
        for index in waiting {
            self.finish_tool_call(index, out);
        }
        self.blocks_stopped = true;

        for held in std::mem::take(&mut self.held) {
            if held.text.is_empty() {
                continue;
            }
            let index = self.take_block_index();
            if held.thinking {
                push_block_start(out, index, thinking_block(""));
                push_block_delta(
                    out,
                    index,
                    object([
                        ("type", "thinking_delta".into()),
                        ("thinking", held.text.into()),
                    ]),
                );
            } else {
                push_block_start(out, index, text_block(""));
                push_block_delta(
                    out,
                    index,
                    object([("type", "text_delta".into()), ("text", held.text.into())]),
                );
            }
            push_block_stop(out, index);
        }
    }

    fn stop_thinking(&mut self, out: &mut Vec<String>) {
        if let Some(index) = self.thinking_block.take() {
            push_block_stop(out, index);
        }
    }

    fn stop_text(&mut self, out: &mut Vec<String>) {
        if let Some(index) = self.text_block.take() {
            push_block_stop(out, index);
        }
    }

    fn message_delta(&mut self, out: &mut Vec<String>) {
        if self.message_delta_sent {
            return;
        }
        let stop_reason = stop_reason(self.terminal_finish_reason());
        let delta = object([
            ("stop_reason", stop_reason.into()),
            ("stop_sequence", Value::Null),
        ]);
        let data = object([
            ("type", "message_delta".into()),
            ("delta", delta),
            ("usage", usage_object(self.usage)),
        ]);
        push_event(out, "message_delta", &data);
        self.message_delta_sent = true;
    }

    fn message_stop(&mut self, out: &mut Vec<String>) {
        if self.message_stop_sent {
            return;
        }
        push_event(
            out,
            "message_stop",
            &object([("type", "message_stop".into())]),
        );
        self.message_stop_sent = true;
    }

    /// The finish reason the response ends with: a started tool call makes it
    /// `tool_calls`, or `length` if a call's arguments aren't a JSON object,
    /// unless the provider stopped for length or a content filter.
    fn terminal_finish_reason(&self) -> &str {
        match self.finish_reason.as_str() {
            reason @ ("length" | "content_filter") => reason,
            _ if self.saw_tool_call => {
                if self.tool_arguments_valid() {
                    "tool_calls"
                } else {
                    "length"
                }
            }
            "" => "stop",
            reason => reason,
        }
    }

    /// Whether every call's arguments, after repair, are a JSON object.
    /// Missing arguments count as valid; blank ones don't.
    fn tool_arguments_valid(&self) -> bool {
        self.tool_calls.values().all(|call| {
            if call.arguments.is_empty() {
                return true;
            }
            let arguments = call.arguments.trim();
            match arguments {
                "" => false,
                "{}" => true,
                _ => {
                    let fixed = fix_json(arguments);
                    raw::valid(&fixed) && fixed.trim_start().starts_with('{')
                }
            }
        })
    }
}

/// Converts a whole Chat Completions response into one Claude message, given
/// the client's Claude request.
pub fn convert_openai_response_to_claude_non_stream(
    original_request: &Value,
    response: &Value,
) -> Value {
    let names = tool_name_map_from_claude_request(original_request);
    let names = names.as_ref();
    let mut out = message_template(&str_of(response.get("id")), &str_of(response.get("model")));
    let mut stop_reason_set = false;
    let mut has_tool_call = false;
    let mut blocks = Vec::new();

    if let Some(Value::Array(choices)) = response.get("choices")
        && let Some(choice) = choices.first()
    {
        if let Some(reason) = choice.get("finish_reason") {
            out["stop_reason"] = stop_reason(&str_of(Some(reason))).into();
            stop_reason_set = true;
        }
        if let Some(message) = choice.get("message") {
            match message.get("content") {
                Some(Value::Array(parts)) => {
                    let mut text = String::new();
                    let mut thinking = String::new();
                    for part in parts {
                        match &*str_of(part.get("type")) {
                            "text" => {
                                flush(&mut blocks, &mut thinking, thinking_block);
                                text.push_str(&str_of(part.get("text")));
                            }
                            "tool_calls" => {
                                flush(&mut blocks, &mut thinking, thinking_block);
                                flush(&mut blocks, &mut text, text_block);
                                if let Some(Value::Array(calls)) = part.get("tool_calls") {
                                    for call in calls {
                                        has_tool_call = true;
                                        blocks.push(tool_use_block(call, names));
                                    }
                                }
                            }
                            "reasoning" => {
                                flush(&mut blocks, &mut text, text_block);
                                if let Some(reasoning) = part.get("text") {
                                    thinking.push_str(&str_of(Some(reasoning)));
                                }
                            }
                            _ => {
                                flush(&mut blocks, &mut thinking, thinking_block);
                                flush(&mut blocks, &mut text, text_block);
                            }
                        }
                    }
                    flush(&mut blocks, &mut thinking, thinking_block);
                    flush(&mut blocks, &mut text, text_block);
                }
                Some(Value::String(text)) if !text.is_empty() => blocks.push(text_block(text)),
                _ => {}
            }
            blocks.extend(
                reasoning_texts(message)
                    .into_iter()
                    .map(|text| thinking_block(&text)),
            );
            if let Some(Value::Array(calls)) = message.get("tool_calls") {
                for call in calls {
                    has_tool_call = true;
                    blocks.push(tool_use_block(call, names));
                }
            }
        }
    }

    if !blocks.is_empty() {
        out["content"] = Value::Array(blocks);
    }
    if let Some(usage) = response.get("usage") {
        out["usage"] = usage_object(extract_usage(usage));
    }
    if !stop_reason_set {
        out["stop_reason"] = if has_tool_call {
            "tool_use"
        } else {
            "end_turn"
        }
        .into();
    }
    out
}

/// `convertOpenAINonStreamingToAnthropic`: one chunk read as a whole
/// response, for a client that didn't ask for a stream. Unlike
/// [`convert_openai_response_to_claude_non_stream`], content is only read as
/// text, tool names are left as they are, and `stop_reason` stays null
/// without a finish reason.
fn chunk_message(chunk: &Value) -> Value {
    let mut out = message_template(&str_of(chunk.get("id")), &str_of(chunk.get("model")));
    if let Some(Value::Array(choices)) = chunk.get("choices")
        && let Some(choice) = choices.first()
    {
        let message = choice.get("message").unwrap_or(&Value::Null);
        let mut blocks: Vec<Value> = reasoning_texts(message)
            .into_iter()
            .map(|text| thinking_block(&text))
            .collect();
        let text = str_of(message.get("content"));
        if !text.is_empty() {
            blocks.push(text_block(&text));
        }
        if let Some(Value::Array(calls)) = message.get("tool_calls") {
            blocks.extend(calls.iter().map(|call| tool_use_block(call, None)));
        }
        if !blocks.is_empty() {
            out["content"] = Value::Array(blocks);
        }
        if let Some(reason) = choice.get("finish_reason") {
            out["stop_reason"] = stop_reason(&str_of(Some(reason))).into();
        }
    }
    if let Some(usage) = chunk.get("usage") {
        out["usage"] = usage_object(extract_usage(usage));
    }
    out
}

/// The Claude message every response starts from.
fn message_template(id: &str, model: &str) -> Value {
    object([
        ("id", id.into()),
        ("type", "message".into()),
        ("role", "assistant".into()),
        ("model", model.into()),
        ("content", Value::Array(Vec::new())),
        ("stop_reason", Value::Null),
        ("stop_sequence", Value::Null),
        (
            "usage",
            object([("input_tokens", 0.into()), ("output_tokens", 0.into())]),
        ),
    ])
}

/// The first choice: `choices.0`, which gjson also finds in an object with
/// the key `0`.
fn first_choice(chunk: &Value) -> Option<&Value> {
    match chunk.get("choices")? {
        Value::Array(choices) => choices.first(),
        choices @ Value::Object(_) => choices.get("0"),
        _ => None,
    }
}

/// The reasoning in a message or delta: the texts in `reasoning_content`,
/// or else `reasoning`, or else `reasoning_details`.
fn reasoning_texts(message: &Value) -> Vec<String> {
    ["reasoning_content", "reasoning", "reasoning_details"]
        .into_iter()
        .filter_map(|key| message.get(key))
        .map(|node| {
            let mut texts = Vec::new();
            collect_reasoning(node, &mut texts);
            texts
        })
        .find(|texts| !texts.is_empty())
        .unwrap_or_default()
}

/// The non-empty strings in `node`, in an array at any depth, or as the
/// `text` of an object.
fn collect_reasoning(node: &Value, texts: &mut Vec<String>) {
    match node {
        Value::Array(items) => items.iter().for_each(|item| collect_reasoning(item, texts)),
        Value::String(text) if !text.is_empty() => texts.push(text.clone()),
        Value::Object(_) => {
            let text = str_of(node.get("text"));
            if !text.is_empty() {
                texts.push(text.into_owned());
            }
        }
        _ => {}
    }
}

/// A `tool_use` block for a whole tool call. Its input is the call's
/// arguments if, once repaired, they are a JSON object, and `{}` otherwise.
fn tool_use_block(call: &Value, names: Option<&ToolNameMap>) -> Value {
    let name = map_tool_name(names, &str_of(path(call, "function.name")));
    let arguments = fix_json(&str_of(path(call, "function.arguments")));
    let input = Some(arguments)
        .filter(|arguments| raw::valid(arguments))
        .and_then(|arguments| serde_json::from_str::<Value>(&arguments).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(Map::new()));
    object([
        ("type", "tool_use".into()),
        ("id", sanitize_tool_id(&str_of(call.get("id"))).into()),
        ("name", name.into()),
        ("input", input),
    ])
}

fn text_block(text: &str) -> Value {
    object([("type", "text".into()), ("text", text.into())])
}

fn thinking_block(thinking: &str) -> Value {
    object([("type", "thinking".into()), ("thinking", thinking.into())])
}

/// Adds the text gathered in `buffer` as a block, if there is any.
fn flush(blocks: &mut Vec<Value>, buffer: &mut String, block: fn(&str) -> Value) {
    if !buffer.is_empty() {
        blocks.push(block(buffer));
        buffer.clear();
    }
}

/// `mapOpenAIFinishReasonToAnthropic`.
fn stop_reason(finish_reason: &str) -> &'static str {
    match finish_reason {
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        _ => "end_turn",
    }
}

/// `extractOpenAIUsage`: Chat Completions token counts, with cache reads
/// and writes taken out of the input tokens.
fn extract_usage(usage: &Value) -> Usage {
    let count = |key: &str| path(usage, key).map_or(0, int_of);
    let mut input = count("prompt_tokens");
    let output = count("completion_tokens");
    let cached = count("prompt_tokens_details.cached_tokens");
    let mut cache_write = count("prompt_tokens_details.cache_write_tokens");
    if cache_write <= 0 {
        cache_write = count("prompt_tokens_details.cache_creation_tokens");
    }
    let deduct = cached.max(0).saturating_add(cache_write.max(0));
    if deduct > 0 {
        input = if input >= deduct { input - deduct } else { 0 };
    }
    Usage {
        input: input.max(0),
        output,
        cached,
        cache_write,
    }
}

/// Claude's `usage`, with the cache counts only when there are any.
fn usage_object(usage: Usage) -> Value {
    let mut out = object([
        ("input_tokens", usage.input.into()),
        ("output_tokens", usage.output.into()),
    ]);
    if usage.cached > 0 {
        out["cache_read_input_tokens"] = usage.cached.into();
    }
    if usage.cache_write > 0 {
        out["cache_creation_input_tokens"] = usage.cache_write.into();
    }
    out
}

fn push_event(out: &mut Vec<String>, event: &str, data: &Value) {
    out.push(format!("event: {event}\ndata: {data}\n\n"));
}

fn push_block_start(out: &mut Vec<String>, index: i64, content_block: Value) {
    let data = object([
        ("type", "content_block_start".into()),
        ("index", index.into()),
        ("content_block", content_block),
    ]);
    push_event(out, "content_block_start", &data);
}

fn push_block_delta(out: &mut Vec<String>, index: i64, delta: Value) {
    let data = object([
        ("type", "content_block_delta".into()),
        ("index", index.into()),
        ("delta", delta),
    ]);
    push_event(out, "content_block_delta", &data);
}

fn push_block_stop(out: &mut Vec<String>, index: i64) {
    let data = object([
        ("type", "content_block_stop".into()),
        ("index", index.into()),
    ]);
    push_event(out, "content_block_stop", &data);
}

#[cfg(test)]
mod tests;
