// Ported from CLIProxyAPI internal/translator/codex/claude/codex_claude_response.go and
// codex_claude_response_web_search.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Codex (OpenAI Responses) events → Claude Messages responses.
//!
//! Codex always streams. [`CodexToClaudeStream`] turns each line of its event
//! stream into Claude SSE events, and [`convert_codex_response_to_claude_non_stream`]
//! turns the final `response.completed` event into one Claude message.
//!
//! Deviations from upstream:
//! - A `data:` line that is not valid JSON is treated as an event with no
//!   fields. gjson reads what it can from malformed JSON.
//! - Where upstream writes JSON into a string, we write the same JSON as
//!   serde_json formats it. This applies to the `partial_json` of a web search
//!   query, where Go also escapes `<`, `>` and `&`, and to a non-string
//!   reasoning summary or message content copied as text.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;

use serde_json::{Map, Value, json};

use super::request::{build_tool_name_map, shorten_call_id};
use crate::common::claude::sanitize_tool_id;
use crate::json::{int_of, path, str_of};

/// Stands in for a missing event or item, as an empty gjson result does upstream.
static NONE: Value = Value::Null;

/// Joins the summary parts of one reasoning item inside its single thinking block.
const SUMMARY_PART_SEPARATOR: &str = "\n\n";

/// Converts a Claude `count_tokens` result into its response body.
pub fn claude_token_count(count: i64) -> Value {
    json!({ "input_tokens": count })
}

/// Translates a Codex event stream into Claude Messages SSE events, one line
/// at a time. Keep one per response: it tracks which content blocks are open.
pub struct CodexToClaudeStream {
    /// Codex tool name → the name the client declared.
    tool_names: HashMap<String, String>,
    block_index: usize,
    has_emitted_tool_use: bool,
    has_text_delta: bool,
    text_open: bool,
    thinking_open: bool,
    thinking_signature: String,
    thinking_summary_seen: bool,
    web_search_tool_use_ids: HashSet<String>,
    web_search_result_ids: HashSet<String>,
    last_web_search_id: String,
    /// Function calls in this response. The fields below hold indices into it.
    calls: Vec<FunctionCall>,
    /// Every key an event may identify a call by (output index, call ID or item ID).
    call_keys: HashMap<String, usize>,
    /// Calls waiting for their turn. Claude content blocks can't interleave, so
    /// one call streams at a time and later calls buffer their arguments.
    queue: VecDeque<usize>,
    active_call: Option<usize>,
    last_call: Option<usize>,
    /// Events that arrived while a call was streaming, replayed after it closes.
    deferred: Vec<Value>,
}

#[derive(Default)]
struct FunctionCall {
    call_id: String,
    name: String,
    block_index: usize,
    arguments: String,
    /// Bytes of `arguments` already sent.
    emitted: usize,
    has_arguments_delta: bool,
    emit_empty_delta: bool,
    started: bool,
    done: bool,
    closed: bool,
}

impl CodexToClaudeStream {
    /// `original_request` is the client's Claude request, used to restore tool
    /// names that were shortened for Codex.
    pub fn new(original_request: &Value) -> Self {
        let tool_names = build_tool_name_map(original_request.get("tools"))
            .into_iter()
            .map(|(original, short)| (short, original))
            .collect();
        Self {
            tool_names,
            block_index: 0,
            has_emitted_tool_use: false,
            has_text_delta: false,
            text_open: false,
            thinking_open: false,
            thinking_signature: String::new(),
            thinking_summary_seen: false,
            web_search_tool_use_ids: HashSet::new(),
            web_search_result_ids: HashSet::new(),
            last_web_search_id: String::new(),
            calls: Vec::new(),
            call_keys: HashMap::new(),
            queue: VecDeque::new(),
            active_call: None,
            last_call: None,
            deferred: Vec::new(),
        }
    }

    /// Translates one line of the Codex event stream. Returns zero or more
    /// complete Claude SSE events (`event: …\ndata: …\n\n`); lines other than
    /// `data:` lines produce nothing.
    pub fn translate_line(&mut self, line: &[u8]) -> String {
        let Some(data) = line.strip_prefix(b"data:") else {
            return String::new();
        };
        let event = std::str::from_utf8(data)
            .ok()
            .and_then(|data| serde_json::from_str(data.trim()).ok())
            .unwrap_or(Value::Null);
        self.translate_event(event)
    }

    fn translate_event(&mut self, event: Value) -> String {
        let kind = str_of(event.get("type")).into_owned();
        if self.active_call.is_some() && should_defer(&kind, &event) {
            self.deferred.push(event);
            return String::new();
        }

        let mut out = String::new();
        if self.handle(&kind, &event, &mut out) && self.queue.is_empty() {
            self.replay_deferred(&mut out);
        }
        out
    }

    /// Handles one event. Returns false where upstream returns early, skipping
    /// the replay of deferred events.
    fn handle(&mut self, kind: &str, event: &Value, out: &mut String) -> bool {
        match kind {
            "error" => push_event(out, "error", &error_event(event)),
            "response.created" => {
                let message = json!({
                    "type": "message_start",
                    "message": {
                        "id": str_of(path(event, "response.id")),
                        "type": "message",
                        "role": "assistant",
                        "model": str_of(path(event, "response.model")),
                        "stop_sequence": null,
                        "usage": { "input_tokens": 0, "output_tokens": 0 },
                        "content": [],
                        "stop_reason": null
                    }
                });
                push_event(out, "message_start", &message);
            }
            "response.reasoning_summary_part.added" => {
                self.stop_text(out);
                // One reasoning item arrives as several summary parts, but only its
                // output_item.done carries the final signature. Keep a single
                // thinking block open for the whole item.
                if self.thinking_open {
                    self.thinking_delta(SUMMARY_PART_SEPARATOR, out);
                } else {
                    self.start_thinking(out);
                }
                self.thinking_summary_seen = true;
            }
            "response.reasoning_summary_text.delta" => {
                self.stop_text(out);
                self.start_thinking(out);
                self.thinking_delta(&str_of(event.get("delta")), out);
            }
            "response.content_part.added" => {
                self.finalize_thinking(out);
                if str_of(path(event, "part.type")) == "output_text" {
                    self.start_text(out);
                }
            }
            "response.output_text.delta" => {
                self.has_text_delta = true;
                self.finalize_thinking(out);
                self.start_text(out);
                self.text_delta(&str_of(event.get("delta")), out);
            }
            "response.content_part.done" => {
                if str_of(path(event, "part.type")) == "output_text" {
                    self.stop_text(out);
                }
            }
            "response.completed" | "response.incomplete" => self.finish(event, out),
            "response.output_item.added" => self.item_added(event, out),
            "response.output_item.done" => return self.item_done(event, out),
            "response.function_call_arguments.delta" => {
                let call = self.call_for_event(event, &NONE);
                self.update_arguments(call, &str_of(event.get("delta")), true);
                self.flush_arguments(call, out);
            }
            "response.function_call_arguments.done" => {
                let call = self.call_for_event(event, &NONE);
                self.update_arguments(call, &str_of(event.get("arguments")), false);
                self.flush_arguments(call, out);
            }
            // Includes reasoning_summary_part.done, which leaves the thinking block
            // open for the signature, and the web_search_call progress events: the
            // populated web_search_call item arrives in output_item.done.
            _ => {}
        }
        true
    }

    fn item_added(&mut self, event: &Value, out: &mut String) {
        let item = event.get("item").unwrap_or(&NONE);
        match &*str_of(item.get("type")) {
            "function_call" => {
                self.finalize_thinking(out);
                self.stop_text(out);
                let call = self.record_call(event, item);
                self.update_identity(call, event, item);
                if !self.calls[call].name.is_empty() {
                    self.calls[call].emit_empty_delta = true;
                }
                self.drain_call_queue(out);
            }
            "reasoning" => {
                self.stop_text(out);
                // A reasoning item that never reported output_item.done must not
                // leave its block open into this one.
                self.finalize_thinking(out);
                self.thinking_summary_seen = false;
                // A fallback for when output_item.done has no encrypted_content.
                self.thinking_signature = str_of(item.get("encrypted_content")).into_owned();
            }
            _ => {}
        }
    }

    fn item_done(&mut self, event: &Value, out: &mut String) -> bool {
        let item = event.get("item").unwrap_or(&NONE);
        match &*str_of(item.get("type")) {
            "message" => {
                // Text already streamed as deltas. Otherwise the message carries it.
                if self.has_text_delta {
                    return false;
                }
                let Some(Value::Array(parts)) = item.get("content") else {
                    return false;
                };
                let text: String = parts
                    .iter()
                    .filter(|part| str_of(part.get("type")) == "output_text")
                    .map(|part| str_of(part.get("text")))
                    .collect();
                if text.is_empty() {
                    return false;
                }
                self.finalize_thinking(out);
                self.start_text(out);
                self.text_delta(&text, out);
                self.stop_text(out);
                self.has_text_delta = true;
            }
            "function_call" => {
                self.finalize_thinking(out);
                self.stop_text(out);
                let call = self.call_for_event(event, item);
                self.update_identity(call, event, item);
                self.update_arguments(call, &str_of(item.get("arguments")), false);
                self.calls[call].done = true;
                self.drain_call_queue(out);
            }
            "reasoning" => {
                self.stop_text(out);
                let signature = str_of(item.get("encrypted_content"));
                if !signature.is_empty() {
                    self.thinking_signature = signature.into_owned();
                }
                if self.thinking_summary_seen {
                    self.finalize_thinking(out);
                } else if !self.thinking_signature.is_empty() {
                    // No summary streamed, but the signature still needs a block.
                    self.start_thinking(out);
                    self.finalize_thinking(out);
                }
                self.thinking_signature.clear();
                self.thinking_summary_seen = false;
            }
            "web_search_call" => self.web_search_result(event, item, out),
            _ => {}
        }
        true
    }

    /// Ends the message: flushes open blocks, function calls listed only in the
    /// final response and deferred events, then reports the stop reason and usage.
    fn finish(&mut self, event: &Value, out: &mut String) {
        let response = event.get("response").unwrap_or(&NONE);
        self.finalize_thinking(out);
        self.stop_text(out);
        self.calls_from_response(response, out);
        self.replay_deferred(out);
        self.finalize_thinking(out);
        self.stop_text(out);

        let stop_reason =
            claude_stop_reason(&codex_stop_reason(response), self.has_emitted_tool_use);
        let delta = json!({
            "type": "message_delta",
            "delta": { "stop_reason": stop_reason, "stop_sequence": stop_sequence(response) },
            "usage": claude_usage(response.get("usage"))
        });
        push_event(out, "message_delta", &delta);
        push_event(out, "message_stop", &json!({ "type": "message_stop" }));
    }

    fn replay_deferred(&mut self, out: &mut String) {
        for event in std::mem::take(&mut self.deferred) {
            out.push_str(&self.translate_event(event));
        }
    }

    fn start_text(&mut self, out: &mut String) {
        if self.text_open {
            return;
        }
        let start = json!({
            "type": "content_block_start",
            "index": self.block_index,
            "content_block": { "type": "text", "text": "" }
        });
        push_event(out, "content_block_start", &start);
        self.text_open = true;
    }

    fn text_delta(&self, text: &str, out: &mut String) {
        let delta = json!({
            "type": "content_block_delta",
            "index": self.block_index,
            "delta": { "type": "text_delta", "text": text }
        });
        push_event(out, "content_block_delta", &delta);
    }

    fn stop_text(&mut self, out: &mut String) {
        if !self.text_open {
            return;
        }
        push_block_stop(out, self.block_index);
        self.text_open = false;
        self.block_index += 1;
    }

    fn start_thinking(&mut self, out: &mut String) {
        if self.thinking_open {
            return;
        }
        let start = json!({
            "type": "content_block_start",
            "index": self.block_index,
            "content_block": { "type": "thinking", "thinking": "" }
        });
        push_event(out, "content_block_start", &start);
        self.thinking_open = true;
    }

    fn thinking_delta(&self, text: &str, out: &mut String) {
        if text.is_empty() {
            return;
        }
        let delta = json!({
            "type": "content_block_delta",
            "index": self.block_index,
            "delta": { "type": "thinking_delta", "thinking": text }
        });
        push_event(out, "content_block_delta", &delta);
    }

    fn finalize_thinking(&mut self, out: &mut String) {
        if !self.thinking_open {
            return;
        }
        if !self.thinking_signature.is_empty() {
            let delta = json!({
                "type": "content_block_delta",
                "index": self.block_index,
                "delta": { "type": "signature_delta", "signature": self.thinking_signature }
            });
            push_event(out, "content_block_delta", &delta);
        }
        push_block_stop(out, self.block_index);
        self.block_index += 1;
        self.thinking_open = false;
    }
}

/// Function calls.
impl CodexToClaudeStream {
    fn find_call(&self, keys: &[String]) -> Option<usize> {
        keys.iter().find_map(|key| self.call_keys.get(key).copied())
    }

    /// The call an event refers to, recorded as a new call if it is unknown.
    /// An event with no identifying keys refers to the last call seen.
    fn call_for_event(&mut self, event: &Value, item: &Value) -> usize {
        let keys = call_keys(event, item);
        let found = if keys.is_empty() {
            self.last_call
        } else {
            self.find_call(&keys)
        };
        found.unwrap_or_else(|| self.record_call(event, item))
    }

    fn record_call(&mut self, event: &Value, item: &Value) -> usize {
        let keys = call_keys(event, item);
        let call = self.find_call(&keys).unwrap_or_else(|| self.new_call());
        self.add_aliases(call, keys);
        self.last_call = Some(call);
        call
    }

    fn new_call(&mut self) -> usize {
        self.calls.push(FunctionCall::default());
        let call = self.calls.len() - 1;
        self.queue.push_back(call);
        call
    }

    fn add_aliases(&mut self, call: usize, keys: Vec<String>) {
        for key in keys {
            self.call_keys.insert(key, call);
        }
    }

    fn update_identity(&mut self, call: usize, event: &Value, item: &Value) {
        let call_id = str_of(item.get("call_id"));
        if !call_id.is_empty() {
            self.calls[call].call_id = call_id.into_owned();
        }
        let name = str_of(item.get("name"));
        if !name.is_empty() {
            self.calls[call].name = name.into_owned();
        }
        self.add_aliases(call, call_keys(event, item));
    }

    /// Appends a delta, or takes complete arguments unless they would discard
    /// streamed ones.
    fn update_arguments(&mut self, call: usize, arguments: &str, delta: bool) {
        if arguments.is_empty() {
            return;
        }
        let call = &mut self.calls[call];
        if delta {
            call.arguments.push_str(arguments);
            call.has_arguments_delta = true;
        } else if !call.has_arguments_delta || arguments.starts_with(&call.arguments) {
            call.arguments = arguments.to_owned();
        }
    }

    /// Sends the arguments a streaming call has buffered since its last delta.
    fn flush_arguments(&mut self, call: usize, out: &mut String) {
        if self.active_call != Some(call) {
            return;
        }
        let call = &mut self.calls[call];
        if !call.started || call.closed || call.emitted >= call.arguments.len() {
            return;
        }
        // Arguments replaced after some were sent can leave this offset inside a
        // character. Go writes each stray byte as U+FFFD, as from_utf8_lossy does.
        let rest = String::from_utf8_lossy(&call.arguments.as_bytes()[call.emitted..]);
        push_input_json_delta(out, call.block_index, &rest);
        call.emitted = call.arguments.len();
    }

    /// Closes the streaming call once it is done and starts the next named one,
    /// until a call is still receiving arguments or none are left.
    fn drain_call_queue(&mut self, out: &mut String) {
        loop {
            if let Some(active) = self.active_call {
                self.flush_arguments(active, out);
                let call = &mut self.calls[active];
                if !call.done {
                    return;
                }
                push_block_stop(out, call.block_index);
                self.block_index = self.block_index.max(call.block_index + 1);
                call.closed = true;
                self.active_call = None;
                if let Some(position) = self.queue.iter().position(|&queued| queued == active) {
                    self.queue.remove(position);
                }
            }

            while self
                .queue
                .front()
                .is_some_and(|&call| self.calls[call].closed)
            {
                self.queue.pop_front();
            }
            let Some(&next) = self.queue.front() else {
                return;
            };
            let call = &mut self.calls[next];
            if call.name.is_empty() {
                return;
            }
            call.block_index = self.block_index;
            let name = self.tool_names.get(&call.name).unwrap_or(&call.name);
            let start = json!({
                "type": "content_block_start",
                "index": call.block_index,
                "content_block": {
                    "type": "tool_use",
                    "id": shorten_call_id(&sanitize_tool_id(&call.call_id)),
                    "name": name,
                    "input": {}
                }
            });
            push_event(out, "content_block_start", &start);
            if call.emit_empty_delta {
                push_input_json_delta(out, call.block_index, "");
            }
            call.started = true;
            self.active_call = Some(next);
            self.has_emitted_tool_use = true;
            self.flush_arguments(next, out);
        }
    }

    /// Completes calls from the final response's `output`, which may list calls
    /// the stream never announced, then streams every named call and forgets them.
    fn calls_from_response(&mut self, response: &Value, out: &mut String) {
        // gjson iterates an object's values too, keyed by name instead of index.
        let items: Vec<(String, &Value)> = match response.get("output") {
            Some(Value::Array(items)) => items
                .iter()
                .enumerate()
                .map(|(index, item)| (index.to_string(), item))
                .collect(),
            Some(Value::Object(items)) => items
                .iter()
                .map(|(key, item)| (key.clone(), item))
                .collect(),
            _ => Vec::new(),
        };
        for (index, item) in items {
            if str_of(item.get("type")) != "function_call" {
                continue;
            }
            let mut keys = call_keys(&NONE, item);
            if let Some(output_index) = item.get("output_index") {
                push_unique(&mut keys, format!("output:{output_index}"));
            }
            push_unique(&mut keys, format!("output:{index}"));
            let call = self.find_call(&keys).unwrap_or_else(|| self.new_call());
            self.add_aliases(call, keys);
            self.update_identity(call, &NONE, item);
            self.update_arguments(call, &str_of(item.get("arguments")), false);
            self.calls[call].done = true;
        }

        let calls = &mut self.calls;
        self.queue.retain(|&call| {
            let call = &mut calls[call];
            if call.closed {
                return false;
            }
            if call.name.is_empty() {
                call.closed = true;
                return false;
            }
            call.done = true;
            true
        });
        self.drain_call_queue(out);

        self.calls.clear();
        self.call_keys.clear();
        self.queue.clear();
        self.active_call = None;
        self.last_call = None;
    }
}

/// Web search.
impl CodexToClaudeStream {
    /// Emits the `server_tool_use` block for a search and its
    /// `web_search_tool_result`, each once per search ID.
    fn web_search_result(&mut self, event: &Value, item: &Value, out: &mut String) {
        let id = self.web_search_id(event, item);
        let query = web_search_query(event, item);
        self.web_search_tool_use(&id, &query, out);
        if self.web_search_result_ids.contains(&id) {
            return;
        }
        let results = web_search_results(event, item);
        if query.is_empty() && results.is_none() && item.get("action").is_none() {
            return;
        }

        let start = json!({
            "type": "content_block_start",
            "index": self.block_index,
            "content_block": {
                "type": "web_search_tool_result",
                "tool_use_id": id,
                "content": results.unwrap_or_default()
            }
        });
        push_event(out, "content_block_start", &start);
        push_block_stop(out, self.block_index);
        self.block_index += 1;
        if id == self.last_web_search_id {
            self.last_web_search_id.clear();
        }
        self.web_search_result_ids.insert(id);
    }

    fn web_search_tool_use(&mut self, id: &str, query: &str, out: &mut String) {
        let started = self.web_search_tool_use_ids.contains(id);
        if started && query.is_empty() {
            return;
        }
        if !started {
            self.stop_text(out);
            self.finalize_thinking(out);
            let start = json!({
                "type": "content_block_start",
                "index": self.block_index,
                "content_block": { "type": "server_tool_use", "id": id, "name": "web_search", "input": {} }
            });
            push_event(out, "content_block_start", &start);
        }
        if !query.is_empty() {
            // Upstream sends a query for a search it already started at the
            // current index, which by then belongs to a later block.
            let input = json!({ "query": query }).to_string();
            push_input_json_delta(out, self.block_index, &input);
        }
        if !started {
            push_block_stop(out, self.block_index);
            self.web_search_tool_use_ids.insert(id.to_owned());
            self.block_index += 1;
        }
    }

    /// The search's ID from the item or event. A search without one reuses the
    /// last generated ID until its result is sent.
    fn web_search_id(&mut self, event: &Value, item: &Value) -> String {
        let field = |key: &str| {
            [item, event]
                .into_iter()
                .map(|source| str_of(source.get(key)).trim().to_owned())
                .find(|value| !value.is_empty())
        };
        if let Some(id) = ["id", "output_item_id", "call_id"]
            .into_iter()
            .find_map(field)
        {
            return id;
        }
        if !self.last_web_search_id.is_empty() {
            return self.last_web_search_id.clone();
        }
        if let Some(id) = field("item_id") {
            return id;
        }
        self.last_web_search_id = format!("web_search_{}", self.block_index);
        self.last_web_search_id.clone()
    }
}

/// Converts the final event of a Codex response (`response.completed` or
/// `response.incomplete`) into a Claude Messages response. Returns `None` for
/// any other event.
pub fn convert_codex_response_to_claude_non_stream(
    original_request: &Value,
    event: &Value,
) -> Option<Value> {
    let kind = str_of(event.get("type"));
    if kind != "response.completed" && kind != "response.incomplete" {
        return None;
    }
    let response = event.get("response")?;
    let tool_names: HashMap<String, String> = build_tool_name_map(original_request.get("tools"))
        .into_iter()
        .map(|(original, short)| (short, original))
        .collect();

    let mut content = Vec::new();
    let mut has_tool_call = false;
    let mut web_searches = HashSet::new();
    let items = match response.get("output") {
        Some(Value::Array(items)) => items.as_slice(),
        _ => &[],
    };
    for item in items {
        match &*str_of(item.get("type")) {
            "reasoning" => {
                let mut thinking = joined_text(item.get("summary"));
                if thinking.is_empty() {
                    thinking = joined_text(item.get("content"));
                }
                let signature = str_of(item.get("encrypted_content"));
                if !thinking.is_empty() || !signature.is_empty() {
                    let mut block = json!({ "type": "thinking", "thinking": thinking });
                    if !signature.is_empty() {
                        block["signature"] = signature.into();
                    }
                    content.push(block);
                }
            }
            "message" => match item.get("content") {
                Some(Value::Array(parts)) => {
                    for part in parts {
                        let text = str_of(part.get("text"));
                        if str_of(part.get("type")) == "output_text" && !text.is_empty() {
                            content.push(json!({ "type": "text", "text": text }));
                        }
                    }
                }
                other => {
                    let text = str_of(other);
                    if !text.is_empty() {
                        content.push(json!({ "type": "text", "text": text }));
                    }
                }
            },
            "web_search_call" => push_web_search_blocks(&mut content, item, &mut web_searches),
            "function_call" => {
                has_tool_call = true;
                let name = str_of(item.get("name"));
                let name = tool_names.get(&*name).map_or(&*name, String::as_str);
                let input = serde_json::from_str::<Value>(&str_of(item.get("arguments")))
                    .ok()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({}));
                content.push(json!({
                    "type": "tool_use",
                    "id": shorten_call_id(&sanitize_tool_id(&str_of(item.get("call_id")))),
                    "name": name,
                    "input": input
                }));
            }
            _ => {}
        }
    }

    Some(json!({
        "id": str_of(response.get("id")),
        "type": "message",
        "role": "assistant",
        "model": str_of(response.get("model")),
        "content": content,
        "stop_reason": claude_stop_reason(&codex_stop_reason(response), has_tool_call),
        "stop_sequence": stop_sequence(response),
        "usage": claude_usage(response.get("usage"))
    }))
}

/// The text of a reasoning summary or content: each part's `text`, or the
/// part itself when it has none.
fn joined_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| str_of(part.get("text").or(Some(part))))
            .collect(),
        other => str_of(other).into_owned(),
    }
}

fn push_web_search_blocks(content: &mut Vec<Value>, item: &Value, seen: &mut HashSet<String>) {
    let id = str_of(item.get("id")).trim().to_owned();
    if id.is_empty() || seen.contains(&id) {
        return;
    }
    let query = web_search_query(&NONE, item);
    let results = web_search_results(&NONE, item);
    if query.is_empty() && results.is_none() {
        return;
    }
    let input = if query.is_empty() {
        json!({})
    } else {
        json!({ "query": query })
    };
    content
        .push(json!({ "type": "server_tool_use", "id": id, "name": "web_search", "input": input }));
    content.push(json!({
        "type": "web_search_tool_result",
        "tool_use_id": id,
        "content": results.unwrap_or_default()
    }));
    seen.insert(id);
}

fn web_search_query(event: &Value, item: &Value) -> String {
    ["action.query", "query", "input.query"]
        .into_iter()
        .flat_map(|key| [item, event].map(|source| str_of(path(source, key)).trim().to_owned()))
        .find(|query| !query.is_empty())
        .unwrap_or_default()
}

/// The search results as Claude `web_search_result` blocks, or `None` when
/// neither the item nor the event has a `results` array.
fn web_search_results(event: &Value, item: &Value) -> Option<Vec<Value>> {
    let results = match (item.get("results"), event.get("results")) {
        (Some(Value::Array(results)), _) | (_, Some(Value::Array(results))) => results,
        _ => return None,
    };
    let blocks = results.iter().filter_map(|result| {
        let url = str_of(result.get("url")).trim().to_owned();
        if url.is_empty() {
            return None;
        }
        let title = str_of(result.get("title")).trim().to_owned();
        let title = if title.is_empty() { url.clone() } else { title };
        Some(json!({ "type": "web_search_result", "title": title, "url": url, "page_age": null }))
    });
    Some(blocks.collect())
}

/// Whether to hold an event back while a function call streams. Claude blocks
/// can't interleave, so only the call's own events and the end go through.
fn should_defer(kind: &str, event: &Value) -> bool {
    match kind {
        "error"
        | "response.completed"
        | "response.incomplete"
        | "response.function_call_arguments.delta"
        | "response.function_call_arguments.done" => false,
        "response.output_item.added" | "response.output_item.done" => {
            str_of(path(event, "item.type")) != "function_call"
        }
        _ => true,
    }
}

/// The keys an event may identify a function call by, without duplicates.
/// `output_index` is used as written, so `1` and `1.0` are different keys.
fn call_keys(event: &Value, item: &Value) -> Vec<String> {
    let mut keys = Vec::with_capacity(5);
    if let Some(output_index) = event.get("output_index") {
        push_unique(&mut keys, format!("output:{output_index}"));
    }
    let ids = [
        ("call:", item.get("call_id")),
        ("call:", event.get("call_id")),
        ("item:", item.get("id")),
        ("item:", event.get("item_id")),
    ];
    for (prefix, id) in ids {
        let id = str_of(id);
        if !id.is_empty() {
            push_unique(&mut keys, format!("{prefix}{id}"));
        }
    }
    keys
}

fn push_unique(keys: &mut Vec<String>, key: String) {
    if !keys.contains(&key) {
        keys.push(key);
    }
}

fn error_event(event: &Value) -> Value {
    let field = |key: &str| str_of(path(event, key)).trim().to_owned();
    let mut kind = [field("error.type"), field("error_type")]
        .into_iter()
        .find(|kind| !kind.is_empty())
        .unwrap_or_else(|| "api_error".to_owned());
    let code = field("error.code");
    let message = [
        field("error.message"),
        field("message"),
        code.clone(),
        kind.clone(),
    ]
    .into_iter()
    .find(|message| !message.is_empty())
    .unwrap_or_default();
    if code == "cyber_policy" || kind == "invalid_request" {
        kind = "invalid_request_error".to_owned();
    }
    json!({ "type": "error", "error": { "type": kind, "message": message } })
}

fn codex_stop_reason(response: &Value) -> String {
    let has_stop_sequence = !str_of(response.get("stop_sequence")).is_empty();
    let stop_reason = str_of(response.get("stop_reason"));
    if !stop_reason.is_empty() {
        if stop_reason == "stop" && has_stop_sequence {
            return "stop_sequence".to_owned();
        }
        return stop_reason.into_owned();
    }
    let incomplete = str_of(path(response, "incomplete_details.reason"));
    if !incomplete.is_empty() {
        return incomplete.into_owned();
    }
    if has_stop_sequence {
        "stop_sequence".to_owned()
    } else {
        String::new()
    }
}

fn claude_stop_reason(stop_reason: &str, has_tool_call: bool) -> &'static str {
    if has_tool_call {
        return "tool_use";
    }
    match stop_reason {
        "max_tokens" | "max_output_tokens" => "max_tokens",
        "stop_sequence" => "stop_sequence",
        "pause_turn" => "pause_turn",
        "refusal" | "content_filter" => "refusal",
        "model_context_window_exceeded" => "model_context_window_exceeded",
        _ => "end_turn",
    }
}

/// The response's `stop_sequence` as Codex sent it, or null when it is empty.
fn stop_sequence(response: &Value) -> Value {
    match response.get("stop_sequence") {
        Some(sequence) if !str_of(Some(sequence)).is_empty() => sequence.clone(),
        _ => Value::Null,
    }
}

/// Converts Responses usage to Claude usage. Claude counts cached and
/// cache-write tokens separately from `input_tokens`; Responses includes them.
fn claude_usage(usage: Option<&Value>) -> Value {
    let usage = usage.filter(|usage| !usage.is_null()).unwrap_or(&NONE);
    let int = |key: &str| path(usage, key).map_or(0, int_of);
    let input = int("input_tokens");
    let output = int("output_tokens");
    let cached = int("input_tokens_details.cached_tokens");
    let mut cache_write = int("input_tokens_details.cache_write_tokens");
    if cache_write <= 0 {
        cache_write = int("input_tokens_details.cache_creation_tokens");
    }
    let counted = cached.max(0).saturating_add(cache_write.max(0));
    let input = if input >= counted { input - counted } else { 0 };

    let mut out = Map::new();
    out.insert("input_tokens".into(), input.max(0).into());
    out.insert("output_tokens".into(), output.into());
    if cached > 0 {
        out.insert("cache_read_input_tokens".into(), cached.into());
    }
    if cache_write > 0 {
        out.insert("cache_creation_input_tokens".into(), cache_write.into());
    }
    if let Some(thinking) = thinking_tokens(usage, output) {
        out.insert(
            "output_tokens_details".into(),
            json!({ "thinking_tokens": thinking }),
        );
    }
    Value::Object(out)
}

/// `reasoning_tokens` as Claude's `thinking_tokens`, capped at the output
/// tokens. Absent unless it is a non-negative number.
fn thinking_tokens(usage: &Value, output_tokens: i64) -> Option<i64> {
    let reasoning = path(usage, "output_tokens_details.reasoning_tokens")?;
    let Value::Number(number) = reasoning else {
        return None;
    };
    let text = number.to_string();
    if text.starts_with('-') {
        return None;
    }
    // Go compares as floats, so a huge count still hits the cap.
    let value: f64 = text.parse().ok()?;
    let cap = output_tokens.max(0);
    Some(if value >= cap as f64 {
        cap
    } else {
        int_of(reasoning)
    })
}

fn push_event(out: &mut String, event: &str, data: &Value) {
    write!(out, "event: {event}\ndata: {data}\n\n").expect("writing to a String cannot fail");
}

fn push_block_stop(out: &mut String, index: usize) {
    push_event(
        out,
        "content_block_stop",
        &json!({ "type": "content_block_stop", "index": index }),
    );
}

fn push_input_json_delta(out: &mut String, index: usize, partial_json: &str) {
    let delta = json!({
        "type": "content_block_delta",
        "index": index,
        "delta": { "type": "input_json_delta", "partial_json": partial_json }
    });
    push_event(out, "content_block_delta", &delta);
}

#[cfg(test)]
mod tests;
