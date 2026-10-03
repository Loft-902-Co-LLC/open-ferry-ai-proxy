// Ported from CLIProxyAPI internal/translator/claude/openai/responses/claude_openai-responses_response.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Claude Messages events → OpenAI Responses events.
//!
//! [`ClaudeToOpenAIResponsesStream`] turns each Claude event into the
//! Responses events it implies, and
//! [`convert_claude_response_to_openai_responses_non_stream`] turns a whole
//! event stream into one Responses object. Output items are numbered in the
//! order they start. Text blocks that follow one another share an assistant
//! message. Thinking blocks become reasoning items; a redacted block's data
//! rides in `encrypted_content` behind a marker. A server-side web search and
//! its result fold into one `web_search_call`.
//!
//! A call to the client's `apply_patch` custom tool streams as custom tool
//! input: the patch text is decoded from the arguments as they arrive.
//! Arguments that aren't one valid input string, a call whose ID or name
//! changes, and a stream that ends early all end the response with
//! `response.failed`.
//!
//! Deviations from upstream:
//! - A `data:` line that is not valid JSON or UTF-8 gives nothing. gjson
//!   reads what it can from malformed JSON. serde_json also rejects an
//!   unpaired surrogate escape such as `\ud800`, which gjson reads as U+FFFD.
//! - serde_json can't read JSON nested more than 128 levels deep either. A
//!   `data:` line that is valid JSON but can't be read for that reason, an
//!   unpaired surrogate escape or bytes that aren't UTF-8 ends the response
//!   with `response.failed` if the request declares `apply_patch`, since the
//!   event could carry part of a patch. Otherwise it gives nothing. Upstream
//!   reads it.
//! - A non-string value read as text is written as compact JSON, where
//!   upstream uses its JSON text. Where a key appears twice in an object, the
//!   last one counts; gjson reads the first. Two things are read as upstream
//!   reads them: a `tool_use` block's `input`, kept as sent so `apply_patch`
//!   snapshot checks see what upstream sees, and the `input` in a custom tool
//!   call's arguments.
//! - Strings are written with serde_json's escaping. Upstream writes `<`, `>`,
//!   `&`, U+2028 and U+2029 in some fields as `\u003c` and so on; the JSON
//!   values are the same.
//! - A token count too large for `i64` saturates. Go's result depends on the
//!   CPU; amd64 gives the minimum `i64`.
//! - Upstream closes open tool calls by walking a Go map, in random order.
//!   They are closed here in block index order.
//! - A client's request that is JSON `null` counts as missing, so the
//!   translated request supplies the tools and the repeated fields. Upstream
//!   takes `null` as the request. The server routes no such request, since it
//!   names no model.
//! - The request's tool declarations, and the fields the final event repeats
//!   from it, are read once, when the stream is created.
//! - A `temperature` or `top_p` that reads as infinite or NaN is written as
//!   `null`. Upstream writes `+Inf` or `NaN`, which isn't JSON.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use super::request::{REDACTED_THINKING_PREFIX, unwrap_custom_tool_input};
use super::tools::RequestTools;
use super::web_search::{
    WEB_SEARCH_TOOL_NAME, build_responses_web_search_call_item, claude_web_search_query,
    claude_web_search_results_to_responses, responses_web_search_call_id,
};
use crate::apply_patch::input::{CallState, InputError, failure};
use crate::apply_patch::is_custom_tool;
use crate::common::request_model_name;
use crate::common::responses::{echo_fields, pick_request};
use crate::json::{go_value, int_of, path, raw, str_of};

/// Translates a Claude event stream into Responses events, one line at a
/// time. Keep one per response.
pub struct ClaudeToOpenAIResponsesStream {
    /// The model `response.created` names: the request's, or the one given
    /// to [`new`](Self::new).
    model: String,
    tools: RequestTools,
    /// The request fields the final event repeats, or `None` without a
    /// request.
    echo: Option<Vec<(&'static str, Value)>>,
    error: Option<ToolInputError>,
    completed: bool,
    seq: i64,
    response_id: String,
    /// When the message started, in Unix seconds.
    created_at: i64,
    next_output_index: i64,
    current_msg_id: String,
    current_fc_id: String,
    in_text_block: bool,
    in_func_block: bool,
    message_open: bool,
    content_part_open: bool,
    /// The open message's output index, or -1 before one is allocated.
    message_output_index: i64,
    /// Tool calls and stopped blocks, by block index.
    funcs: BTreeMap<i64, FuncCall>,
    /// The open message's text.
    text: String,
    /// The open message's citations, as Claude sent them.
    annotations: Vec<Value>,
    messages: Vec<MessageItem>,
    reasoning_active: bool,
    reasoning_deltas_done: bool,
    reasoning_item_id: String,
    reasoning_text: String,
    reasoning_signature: String,
    /// The open reasoning item's output index, or -1.
    reasoning_index: i64,
    reasoning_items: Vec<ReasoningItem>,
    /// Upstream doesn't reset these at `message_start`.
    web_searches: Vec<WebSearch>,
    web_search_by_block: HashMap<i64, usize>,
    web_search_by_tool_id: HashMap<String, usize>,
    stop_reason: String,
    usage: Usage,
}

/// What upstream keeps about one block index in its `Func*` maps.
#[derive(Default)]
struct FuncCall {
    /// Whether a `tool_use` block started here: a key of `FuncCallIDs`.
    tracked: bool,
    call_id: String,
    name: String,
    /// The arguments so far, if a `tool_use` start or an `input_json_delta`
    /// was seen: a key of `FuncArgsBuf`.
    args: Option<String>,
    /// How many bytes of `args` have been sent.
    args_sent: usize,
    item_added: bool,
    block_stopped: bool,
    /// The last non-empty `input` a start of this block carried, as sent.
    input_snapshot: String,
    /// The first snapshot that failed `apply_patch` validation.
    snapshot_error: Option<InputError>,
    identity_conflict: bool,
    args_done: bool,
    item_done: bool,
    item_status: &'static str,
    custom: bool,
    output_index: Option<i64>,
    /// Set when the call is to the `apply_patch` custom tool.
    apply_patch: Option<CallState>,
}

/// `claudeResponsesMessageItem`
struct MessageItem {
    id: String,
    output_index: i64,
    text: String,
    annotations: Vec<Value>,
    status: &'static str,
}

/// `claudeResponsesReasoningItem`
struct ReasoningItem {
    id: String,
    output_index: i64,
    text: String,
    signature: String,
    status: &'static str,
}

/// `claudeResponsesWebSearchItem`: a `server_tool_use` block and its result.
struct WebSearch {
    tool_use_id: String,
    output_index: i64,
    input: String,
    results: Option<Value>,
    emitted: bool,
    status: &'static str,
}

impl WebSearch {
    /// `render`, with `status` set.
    fn item(&self, status: &str) -> Value {
        let mut item = build_responses_web_search_call_item(
            &self.tool_use_id,
            &claude_web_search_query(&self.input),
            self.results.as_ref(),
        );
        item.insert("status".into(), status.into());
        Value::Object(item)
    }
}

/// Why a stream with an `apply_patch` call failed. Upstream keeps it as an
/// `error` for the caller.
#[derive(Debug)]
enum ToolInputError {
    /// The arguments, or a snapshot of them, aren't one valid input string.
    Arguments(InputError),
    /// The call's ID or name changed.
    ConflictingIdentity,
    /// The stream ended before `message_stop`.
    Unterminated,
    /// An event was valid JSON that serde_json can't read. Not upstream's.
    Unreadable,
}

impl fmt::Display for ToolInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Arguments(error) => error.fmt(f),
            Self::ConflictingIdentity => f.write_str("conflicting apply_patch call identity"),
            Self::Unterminated => {
                f.write_str("upstream apply_patch stream ended before protocol completion")
            }
            Self::Unreadable => f.write_str("unreadable upstream event in apply_patch stream"),
        }
    }
}

impl Error for ToolInputError {}

impl ClaudeToOpenAIResponsesStream {
    /// `model` is named in `response.created` when neither request names a
    /// model. `original_request` is the client's Responses request and
    /// `request` the translated one; the first that isn't `Null` supplies the
    /// tool declarations and the fields the final event repeats.
    pub fn new(model: &str, original_request: &Value, request: &Value) -> Self {
        let picked = pick_request(original_request, request);
        Self {
            model: request_model_name(original_request, request)
                .unwrap_or(model)
                .to_owned(),
            tools: RequestTools::new(picked.unwrap_or(&Value::Null)),
            echo: picked.map(|request| echo_fields(request, |_| None)),
            error: None,
            completed: false,
            seq: 0,
            response_id: String::new(),
            created_at: 0,
            next_output_index: 0,
            current_msg_id: String::new(),
            current_fc_id: String::new(),
            in_text_block: false,
            in_func_block: false,
            message_open: false,
            content_part_open: false,
            message_output_index: -1,
            funcs: BTreeMap::new(),
            text: String::new(),
            annotations: Vec::new(),
            messages: Vec::new(),
            reasoning_active: false,
            reasoning_deltas_done: false,
            reasoning_item_id: String::new(),
            reasoning_text: String::new(),
            reasoning_signature: String::new(),
            reasoning_index: -1,
            reasoning_items: Vec::new(),
            web_searches: Vec::new(),
            web_search_by_block: HashMap::new(),
            web_search_by_tool_id: HashMap::new(),
            stop_reason: String::new(),
            usage: Usage::default(),
        }
    }

    /// `ConvertClaudeResponseToOpenAIResponses`: translates one line of the
    /// Claude event stream. Returns the SSE frames to send, `event:` and
    /// `data:` lines each, or `""`. Nothing follows the final event or
    /// `response.failed`.
    pub fn translate_line(&mut self, line: &[u8]) -> String {
        let mut out = String::new();
        if self.error.is_some() || self.completed {
            return out;
        }
        let Some((data, event)) = parse_data_line(line) else {
            if unreadable(line) && patch_enabled(&self.tools) {
                self.fail_tool_input(ToolInputError::Unreadable, &mut out);
            }
            return out;
        };
        let index = event.get("index").map_or(0, int_of);
        match &*str_of(event.get("type")) {
            "message_start" => {
                if let Some(message) = event.get("message") {
                    self.start_message(message, &mut out);
                }
            }
            "content_block_start" => {
                if let Some(block) = event.get("content_block") {
                    self.start_block(index, block, data, &mut out);
                }
            }
            "content_block_delta" => {
                if let Some(delta) = event.get("delta") {
                    self.block_delta(index, delta, &mut out);
                }
            }
            "content_block_stop" => {
                self.funcs.entry(index).or_default().block_stopped = true;
                if self.in_text_block {
                    self.in_text_block = false;
                } else if self.in_func_block {
                    self.in_func_block = false;
                } else if self.reasoning_active {
                    self.finalize_reasoning_deltas(&mut out);
                }
            }
            "message_delta" => {
                self.usage.merge(event.get("usage"));
                if let Some(reason) = path(&event, "delta.stop_reason") {
                    self.stop_reason = str_of(Some(reason)).into_owned();
                }
            }
            "message_stop" => self.stop_message(&mut out),
            _ => {}
        }
        out
    }

    /// `FinalizeToolInput`: call when the Claude stream ends. If the request
    /// declares `apply_patch` and the stream ended before `message_stop`,
    /// returns `response.failed`, since a patch may be cut short.
    pub fn finalize_tool_input(&mut self) -> String {
        let mut out = String::new();
        if self.error.is_some() || self.completed {
            return out;
        }
        if !patch_enabled(&self.tools) {
            return out;
        }
        self.error = Some(ToolInputError::Unterminated);
        self.seq += 1;
        push_event(
            &mut out,
            "response.failed",
            &failure(&self.response_id, self.seq),
        );
        out
    }

    /// `ToolInputError`: why the stream failed, if an `apply_patch` call did.
    pub fn tool_input_error(&self) -> Option<&(dyn Error + 'static)> {
        self.error
            .as_ref()
            .map(|error| error as &(dyn Error + 'static))
    }

    fn start_message(&mut self, message: &Value, out: &mut String) {
        self.response_id = str_of(message.get("id")).into_owned();
        self.created_at = now();
        self.text.clear();
        self.annotations.clear();
        self.messages.clear();
        self.reasoning_text.clear();
        self.reasoning_active = false;
        self.reasoning_deltas_done = false;
        self.next_output_index = 0;
        self.in_text_block = false;
        self.in_func_block = false;
        self.message_open = false;
        self.content_part_open = false;
        self.current_msg_id.clear();
        self.current_fc_id.clear();
        self.message_output_index = -1;
        self.reasoning_item_id.clear();
        self.reasoning_signature.clear();
        self.reasoning_index = -1;
        self.reasoning_items.clear();
        self.stop_reason.clear();
        self.funcs.clear();
        self.usage = Usage::default();
        self.usage.merge(message.get("usage"));

        let mut response = json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": "in_progress",
            "background": false,
            "error": null,
            "output": [],
        });
        if !self.model.is_empty() {
            response["model"] = self.model.clone().into();
        }
        let seq = self.next_seq();
        let created = json!({
            "type": "response.created",
            "sequence_number": seq,
            "response": response,
        });
        push_event(out, "response.created", &created);

        let mut response = json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": "in_progress",
            "output": [],
        });
        if !self.model.is_empty() {
            response["model"] = self.model.clone().into();
        }
        let seq = self.next_seq();
        let in_progress = json!({
            "type": "response.in_progress",
            "sequence_number": seq,
            "response": response,
        });
        push_event(out, "response.in_progress", &in_progress);
    }

    fn start_block(&mut self, index: i64, block: &Value, data: &str, out: &mut String) {
        let kind = str_of(block.get("type"));
        // Adjacent text blocks share an assistant message.
        if kind != "text" {
            self.finalize_assistant_message(out);
        }
        if self.reasoning_active || !self.reasoning_item_id.is_empty() {
            self.finalize_reasoning_item("completed", out);
        }
        for previous in self.tracked_calls() {
            let call = &self.funcs[&previous];
            if call.item_done || previous == index {
                continue;
            }
            // Patch calls may interleave: a new block doesn't complete one
            // that is still open or not yet named.
            if (call.name.is_empty() || is_apply_patch(&self.tools, &call.name))
                && !call.block_stopped
            {
                continue;
            }
            self.finalize_func_item(previous, "completed", out);
            if self.error.is_some() {
                return;
            }
        }
        for position in 0..self.web_searches.len() {
            let search = &self.web_searches[position];
            if !search.emitted && search.results.is_some() {
                self.finalize_web_search(position, "completed", out);
            }
        }

        match &*kind {
            "text" => self.start_text(out),
            "tool_use" => self.start_tool_use(index, block, data, out),
            "server_tool_use" => {
                // Only web search can be enabled on Claude by this
                // translator; any other server tool is left out.
                if str_of(block.get("name")) == WEB_SEARCH_TOOL_NAME {
                    self.start_web_search(index, &str_of(block.get("id")), out);
                }
            }
            "web_search_tool_result" => {
                // The result comes whole, with no deltas. Its item is closed
                // when the next block starts or at `message_stop`, so that
                // the final stop reason counts.
                let tool_use_id = str_of(block.get("tool_use_id"));
                if let Some(&position) = self.web_search_by_tool_id.get(&*tool_use_id) {
                    self.web_searches[position].results =
                        claude_web_search_results_to_responses(block.get("content"));
                }
            }
            "thinking" | "redacted_thinking" => self.start_reasoning(index, block, out),
            _ => {}
        }
    }

    fn start_text(&mut self, out: &mut String) {
        self.in_text_block = true;
        let output_index = self.message_output_index();
        if self.current_msg_id.is_empty() {
            self.current_msg_id = format!("msg_{}_{}", self.response_id, self.messages.len());
        }
        if !self.message_open {
            let seq = self.next_seq();
            let added = json!({
                "type": "response.output_item.added",
                "sequence_number": seq,
                "output_index": output_index,
                "item": {
                    "id": self.current_msg_id,
                    "type": "message",
                    "status": "in_progress",
                    "content": [],
                    "role": "assistant",
                },
            });
            push_event(out, "response.output_item.added", &added);
            self.message_open = true;
        }
        if !self.content_part_open {
            let seq = self.next_seq();
            let part = json!({
                "type": "response.content_part.added",
                "sequence_number": seq,
                "item_id": self.current_msg_id,
                "output_index": output_index,
                "content_index": 0,
                "part": output_text_part("", &[]),
            });
            push_event(out, "response.content_part.added", &part);
            self.content_part_open = true;
        }
    }

    fn start_tool_use(&mut self, index: i64, block: &Value, data: &str, out: &mut String) {
        self.in_func_block = true;
        let call_id = str_of(block.get("id"));
        let name = str_of(block.get("name"));
        let tools = &self.tools;
        let call = self.funcs.entry(index).or_default();
        // Evidence of a conflict must survive later starts that agree.
        if !call_id.is_empty() && !call.call_id.is_empty() && call_id != call.call_id {
            call.identity_conflict = true;
        }
        if (is_apply_patch(tools, &call.name) || is_apply_patch(tools, &name))
            && (call.identity_conflict
                || (!name.is_empty()
                    && !call.name.is_empty()
                    && tools.identity(&name) != tools.identity(&call.name)))
        {
            self.fail_tool_input(ToolInputError::ConflictingIdentity, out);
            return;
        }
        // The block is tracked even before its ID arrives, so that the
        // terminal checks can't overlook an unnamed or ID-less call.
        if !call.item_added && (!call_id.is_empty() || call.call_id.is_empty()) {
            call.call_id = call_id.into_owned();
        }
        call.tracked = true;
        if !name.is_empty() && !call.item_added {
            call.name = name.into_owned();
        }
        self.current_fc_id = call.call_id.clone();
        self.function_output_index(index);
        let call = self.funcs.get_mut(&index).expect("the call was just added");
        call.args.get_or_insert_default();
        // An empty start input is Claude's placeholder, not an arguments
        // fragment. A populated one is kept as a snapshot, not previewed.
        if let Some(input) = block.get("input")
            && !matches!(input, Value::Object(fields) if fields.is_empty())
        {
            let raw = raw::member(data, "content_block")
                .and_then(|block| raw::member(block, "input"))
                .map_or_else(|| input.to_string(), str::to_owned);
            // Validate without finishing the live decoder. A failure waits
            // until the call is known to be a patch.
            if let Err(error) = validate_snapshots(&call.input_snapshot, &raw)
                && call.snapshot_error.is_none()
            {
                call.snapshot_error = Some(error);
            }
            // A finished item doesn't seal the response: a late snapshot is
            // checked against the finished decoder, emitting nothing more.
            if call.item_done
                && let Some(patch) = &mut call.apply_patch
                && let Err(error) = patch.finish_arguments(&raw)
            {
                self.fail_tool_input(ToolInputError::Arguments(error), out);
                return;
            }
            call.input_snapshot = raw;
        }
        if is_apply_patch(&self.tools, &call.name)
            && let Some(error) = call.snapshot_error.clone()
        {
            self.fail_tool_input(ToolInputError::Arguments(error), out);
            return;
        }
        self.with_call(index, |this, call| {
            this.emit_func_item(index, call, false, out);
            this.emit_pending_func_args(call, out);
        });
    }

    /// `startWebSearch`, and the item's `output_item.added`.
    fn start_web_search(&mut self, index: i64, tool_use_id: &str, out: &mut String) {
        let output_index = self.allocate_output_index();
        let position = self.web_searches.len();
        self.web_search_by_block.insert(index, position);
        self.web_search_by_tool_id
            .insert(tool_use_id.to_owned(), position);
        self.web_searches.push(WebSearch {
            tool_use_id: tool_use_id.to_owned(),
            output_index,
            input: String::new(),
            results: None,
            emitted: false,
            status: "",
        });
        let seq = self.next_seq();
        let added = json!({
            "type": "response.output_item.added",
            "sequence_number": seq,
            "output_index": output_index,
            "item": {
                "id": responses_web_search_call_id(tool_use_id),
                "type": "web_search_call",
                "status": "in_progress",
                "action": {"type": "search", "query": ""},
            },
        });
        push_event(out, "response.output_item.added", &added);
    }

    fn start_reasoning(&mut self, index: i64, block: &Value, out: &mut String) {
        self.reasoning_active = true;
        self.reasoning_deltas_done = false;
        self.reasoning_index = self.allocate_output_index();
        self.reasoning_text.clear();
        self.reasoning_signature = reasoning_carrier(block);
        self.reasoning_item_id = format!("rs_{}_{index}", self.response_id);
        let seq = self.next_seq();
        let added = json!({
            "type": "response.output_item.added",
            "sequence_number": seq,
            "output_index": self.reasoning_index,
            "item": {
                "id": self.reasoning_item_id,
                "type": "reasoning",
                "status": "in_progress",
                "encrypted_content": self.reasoning_signature,
                "summary": [],
            },
        });
        push_event(out, "response.output_item.added", &added);
        let seq = self.next_seq();
        let part = json!({
            "type": "response.reasoning_summary_part.added",
            "sequence_number": seq,
            "item_id": self.reasoning_item_id,
            "output_index": self.reasoning_index,
            "summary_index": 0,
            "part": {"type": "summary_text", "text": ""},
        });
        push_event(out, "response.reasoning_summary_part.added", &part);
    }

    fn block_delta(&mut self, index: i64, delta: &Value, out: &mut String) {
        match &*str_of(delta.get("type")) {
            "text_delta" => {
                let Some(text) = delta.get("text") else {
                    return;
                };
                let text = str_of(Some(text));
                let seq = self.next_seq();
                let output_index = self.message_output_index();
                let event = json!({
                    "type": "response.output_text.delta",
                    "sequence_number": seq,
                    "item_id": self.current_msg_id,
                    "output_index": output_index,
                    "content_index": 0,
                    "delta": text,
                    "logprobs": [],
                });
                push_event(out, "response.output_text.delta", &event);
                self.text.push_str(&text);
            }
            "input_json_delta" => {
                let partial = delta.get("partial_json");
                if let Some(&position) = self.web_search_by_block.get(&index) {
                    if let Some(partial) = partial {
                        self.web_searches[position]
                            .input
                            .push_str(&str_of(Some(partial)));
                    }
                    return;
                }
                if let Some(partial) = partial {
                    self.funcs
                        .entry(index)
                        .or_default()
                        .args
                        .get_or_insert_default()
                        .push_str(&str_of(Some(partial)));
                    self.with_call(index, |this, call| this.emit_pending_func_args(call, out));
                }
            }
            "thinking_delta" => {
                if !self.reasoning_active {
                    return;
                }
                let Some(thinking) = delta.get("thinking") else {
                    return;
                };
                let thinking = str_of(Some(thinking));
                self.reasoning_text.push_str(&thinking);
                let seq = self.next_seq();
                let event = json!({
                    "type": "response.reasoning_summary_text.delta",
                    "sequence_number": seq,
                    "item_id": self.reasoning_item_id,
                    "output_index": self.reasoning_index,
                    "summary_index": 0,
                    "delta": thinking,
                });
                push_event(out, "response.reasoning_summary_text.delta", &event);
            }
            // The signature goes out with the item, not as a delta.
            "signature_delta" => {
                let signature = str_of(delta.get("signature"));
                if self.reasoning_active && !signature.is_empty() {
                    self.reasoning_signature = signature.into_owned();
                }
            }
            "citations_delta" => {
                if let Some(citation) = delta.get("citation")
                    && !citation.is_null()
                {
                    self.annotations.push(citation.clone());
                }
            }
            _ => {}
        }
    }

    fn stop_message(&mut self, out: &mut String) {
        let tool_status = output_status(&self.stop_reason);
        if self.reasoning_active || !self.reasoning_item_id.is_empty() {
            self.finalize_reasoning_item(tool_status, out);
        }
        self.finalize_assistant_message(out);
        for index in self.tracked_calls() {
            if !self.funcs[&index].item_done {
                self.finalize_func_item(index, tool_status, out);
                if self.error.is_some() {
                    return;
                }
            }
        }
        for position in 0..self.web_searches.len() {
            if !self.web_searches[position].emitted {
                self.finalize_web_search(position, tool_status, out);
            }
        }

        let (event_type, status) = terminal_state(&self.stop_reason);
        let seq = self.next_seq();
        let mut response = Map::new();
        response.insert("id".into(), self.response_id.clone().into());
        response.insert("object".into(), "response".into());
        response.insert("created_at".into(), self.created_at.into());
        response.insert("status".into(), status.into());
        response.insert("background".into(), false.into());
        response.insert("error".into(), Value::Null);
        if status == "incomplete" {
            response.insert("incomplete_details".into(), incomplete_details());
        }
        for (key, value) in self.echo.iter().flatten() {
            response.insert((*key).into(), value.clone());
        }

        let mut output = Vec::new();
        for item in &self.reasoning_items {
            let status = if item.status.is_empty() {
                "completed"
            } else {
                item.status
            };
            let reasoning = reasoning_item(&item.id, status, &item.signature, &item.text);
            set_output(&mut output, item.output_index, reasoning);
        }
        for item in &self.messages {
            let message = message_item(&item.id, item.status, &item.text, &item.annotations);
            set_output(&mut output, item.output_index, message);
        }
        for search in &self.web_searches {
            let status = if search.status.is_empty() {
                "completed"
            } else {
                search.status
            };
            set_output(&mut output, search.output_index, search.item(status));
        }
        // Upstream lists calls in block index order here too.
        for call in self.funcs.values() {
            let Some(buffer) = &call.args else {
                continue;
            };
            let status = if call.item_status.is_empty() {
                "completed"
            } else {
                call.item_status
            };
            let mut args = if !call.custom && status == "completed" {
                "{}"
            } else {
                ""
            };
            if !buffer.is_empty() {
                args = buffer;
            }
            let call_id = if call.call_id.is_empty() {
                &self.current_fc_id
            } else {
                &call.call_id
            };
            let item = if call.custom {
                let input = match &call.apply_patch {
                    Some(patch) => patch.input().to_owned(),
                    None => unwrap_custom_tool_input(args),
                };
                let id = format!("ctc_{call_id}");
                call_item(&self.tools, true, &id, status, &input, call_id, &call.name)
            } else {
                let id = format!("fc_{call_id}");
                call_item(&self.tools, false, &id, status, args, call_id, &call.name)
            };
            set_output(&mut output, call.output_index.unwrap_or(0), item);
        }
        if !output.is_empty() {
            response.insert("output".into(), Value::Array(output));
        }

        let reasoning_length: usize = self.reasoning_items.iter().map(|r| r.text.len()).sum();
        let reasoning_tokens = (reasoning_length / 4) as i64;
        if self.usage.reported || reasoning_tokens > 0 {
            let (input, output, total, cached) = self.usage.responses();
            let mut usage = json!({
                "input_tokens": input,
                "input_tokens_details": {"cached_tokens": cached},
                "output_tokens": output,
                "output_tokens_details": {"reasoning_tokens": reasoning_tokens},
            });
            if total > 0 || self.usage.reported {
                usage["total_tokens"] = total.into();
            }
            response.insert("usage".into(), usage);
        }
        self.completed = true;
        let event = json!({
            "type": event_type,
            "sequence_number": seq,
            "response": response,
        });
        push_event(out, event_type, &event);
    }

    /// `failToolInput`: keeps the first error and sends `response.failed`.
    fn fail_tool_input(&mut self, error: ToolInputError, out: &mut String) {
        if self.error.is_some() {
            return;
        }
        self.error = Some(error);
        let seq = self.next_seq();
        push_event(out, "response.failed", &failure(&self.response_id, seq));
    }

    /// Runs `f` on the call at block `index`, which is out of `funcs`
    /// meanwhile.
    fn with_call<R>(&mut self, index: i64, f: impl FnOnce(&mut Self, &mut FuncCall) -> R) -> R {
        let mut call = self.funcs.remove(&index).unwrap_or_default();
        let result = f(self, &mut call);
        self.funcs.insert(index, call);
        result
    }

    /// `emitFuncItem`: the call's `output_item.added`, once its name and ID
    /// are known, or at once if `force`d.
    fn emit_func_item(&mut self, index: i64, call: &mut FuncCall, force: bool, out: &mut String) {
        if call.item_added || self.error.is_some() {
            return;
        }
        if force
            && call.name.is_empty()
            && let Some(only) = self.tools.only_name()
            && is_apply_patch(&self.tools, only)
        {
            call.name = self.tools.claude_name(only);
        }
        if is_apply_patch(&self.tools, &call.name) {
            if call.identity_conflict {
                self.fail_tool_input(ToolInputError::ConflictingIdentity, out);
                return;
            }
            if let Some(error) = &call.snapshot_error {
                self.fail_tool_input(ToolInputError::Arguments(error.clone()), out);
                return;
            }
        }
        if !force && (call.name.is_empty() || call.call_id.is_empty()) {
            return;
        }
        if call.call_id.is_empty() {
            call.call_id = format!("call_{}_{index}", self.response_id);
        }
        call.custom = self
            .tools
            .winner(self.tools.identity(&call.name))
            .is_some_and(|d| d.kind == "custom");
        let output_index = self.call_output_index(call);
        let item_id = if call.custom {
            format!("ctc_{}", call.call_id)
        } else {
            format!("fc_{}", call.call_id)
        };
        if call.custom && is_apply_patch(&self.tools, &call.name) {
            call.apply_patch = Some(CallState::new(
                item_id.clone(),
                call.call_id.clone(),
                output_index,
            ));
        }
        let item = call_item(
            &self.tools,
            call.custom,
            &item_id,
            "in_progress",
            "",
            &call.call_id,
            &call.name,
        );
        let seq = self.next_seq();
        let added = json!({
            "type": "response.output_item.added",
            "sequence_number": seq,
            "output_index": output_index,
            "item": item,
        });
        push_event(out, "response.output_item.added", &added);
        call.item_added = true;
    }

    /// `emitPendingFuncArgs`: the arguments that came since the last delta.
    fn emit_pending_func_args(&mut self, call: &mut FuncCall, out: &mut String) {
        if !call.item_added || self.error.is_some() {
            return;
        }
        let Some(args) = &call.args else {
            return;
        };
        if args.len() <= call.args_sent {
            return;
        }
        let fragment = args[call.args_sent..].to_owned();
        call.args_sent = args.len();
        if call.custom {
            if let Some(patch) = &mut call.apply_patch {
                match patch.push_arguments(&fragment) {
                    Err(error) => self.fail_tool_input(ToolInputError::Arguments(error), out),
                    Ok(delta) if !delta.is_empty() => {
                        let seq = self.next_seq();
                        push_event(
                            out,
                            "response.custom_tool_call_input.delta",
                            &patch.input_delta(&delta, seq),
                        );
                    }
                    Ok(_) => {}
                }
            }
            return;
        }
        let seq = self.next_seq();
        let output_index = self.call_output_index(call);
        let event = json!({
            "type": "response.function_call_arguments.delta",
            "sequence_number": seq,
            "item_id": format!("fc_{}", call.call_id),
            "output_index": output_index,
            "delta": fragment,
        });
        push_event(out, "response.function_call_arguments.delta", &event);
    }

    fn finalize_func_item(&mut self, index: i64, status: &'static str, out: &mut String) {
        self.with_call(index, |this, call| {
            this.finalize_call(index, call, status, out);
        });
    }

    /// `finalizeFuncItem`: the call's remaining arguments and its
    /// `output_item.done`.
    fn finalize_call(
        &mut self,
        index: i64,
        call: &mut FuncCall,
        status: &'static str,
        out: &mut String,
    ) {
        if call.item_done || self.error.is_some() {
            return;
        }
        self.emit_func_item(index, call, true, out);
        self.emit_pending_func_args(call, out);
        if self.error.is_some() {
            return;
        }
        call.item_done = true;
        call.item_status = status;
        let output_index = self.call_output_index(call);
        let mut args = call.args.clone().unwrap_or_default();
        if !call.custom && args.is_empty() && status == "completed" {
            args = "{}".to_owned();
        }
        let call_id = if call.call_id.is_empty() {
            self.current_fc_id.clone()
        } else {
            call.call_id.clone()
        };

        if call.custom {
            let input = match &mut call.apply_patch {
                Some(patch) => match finish_patch_arguments(patch, &args, &call.input_snapshot) {
                    Err(error) => {
                        self.fail_tool_input(ToolInputError::Arguments(error), out);
                        return;
                    }
                    Ok((tail, input)) => {
                        if !tail.is_empty() {
                            let seq = self.next_seq();
                            push_event(
                                out,
                                "response.custom_tool_call_input.delta",
                                &patch.input_delta(&tail, seq),
                            );
                        }
                        input
                    }
                },
                None => unwrap_custom_tool_input(&args),
            };
            if !call.args_done {
                call.args_done = true;
                let seq = self.next_seq();
                let done = match &call.apply_patch {
                    Some(patch) => patch.input_done(&input, seq),
                    None => json!({
                        "type": "response.custom_tool_call_input.done",
                        "sequence_number": seq,
                        "item_id": format!("ctc_{call_id}"),
                        "output_index": output_index,
                        "input": input,
                    }),
                };
                push_event(out, "response.custom_tool_call_input.done", &done);
            }
            let id = format!("ctc_{call_id}");
            let item = call_item(&self.tools, true, &id, status, &input, &call_id, &call.name);
            let seq = self.next_seq();
            let done = json!({
                "type": "response.output_item.done",
                "sequence_number": seq,
                "output_index": output_index,
                "item": item,
            });
            push_event(out, "response.output_item.done", &done);
        } else {
            if !call.args_done {
                call.args_done = true;
                let seq = self.next_seq();
                let done = json!({
                    "type": "response.function_call_arguments.done",
                    "sequence_number": seq,
                    "item_id": format!("fc_{call_id}"),
                    "output_index": output_index,
                    "arguments": args,
                });
                push_event(out, "response.function_call_arguments.done", &done);
            }
            let id = format!("fc_{call_id}");
            let item = call_item(&self.tools, false, &id, status, &args, &call_id, &call.name);
            let seq = self.next_seq();
            let done = json!({
                "type": "response.output_item.done",
                "sequence_number": seq,
                "output_index": output_index,
                "item": item,
            });
            push_event(out, "response.output_item.done", &done);
        }
        self.in_func_block = false;
    }

    /// `finalizeReasoningDeltas`
    fn finalize_reasoning_deltas(&mut self, out: &mut String) {
        if !self.reasoning_active || self.reasoning_deltas_done {
            return;
        }
        self.reasoning_deltas_done = true;
        let seq = self.next_seq();
        let text_done = json!({
            "type": "response.reasoning_summary_text.done",
            "sequence_number": seq,
            "item_id": self.reasoning_item_id,
            "output_index": self.reasoning_index,
            "summary_index": 0,
            "text": self.reasoning_text,
        });
        push_event(out, "response.reasoning_summary_text.done", &text_done);
        let seq = self.next_seq();
        let part_done = json!({
            "type": "response.reasoning_summary_part.done",
            "sequence_number": seq,
            "item_id": self.reasoning_item_id,
            "output_index": self.reasoning_index,
            "summary_index": 0,
            "part": {"type": "summary_text", "text": self.reasoning_text},
        });
        push_event(out, "response.reasoning_summary_part.done", &part_done);
    }

    /// `finalizeReasoningItem`
    fn finalize_reasoning_item(&mut self, status: &'static str, out: &mut String) {
        if !self.reasoning_active && self.reasoning_item_id.is_empty() {
            return;
        }
        self.finalize_reasoning_deltas(out);
        let seq = self.next_seq();
        let item = reasoning_item(
            &self.reasoning_item_id,
            status,
            &self.reasoning_signature,
            &self.reasoning_text,
        );
        let done = json!({
            "type": "response.output_item.done",
            "sequence_number": seq,
            "output_index": self.reasoning_index,
            "item": item,
        });
        push_event(out, "response.output_item.done", &done);
        self.reasoning_items.push(ReasoningItem {
            id: std::mem::take(&mut self.reasoning_item_id),
            output_index: self.reasoning_index,
            text: std::mem::take(&mut self.reasoning_text),
            signature: std::mem::take(&mut self.reasoning_signature),
            status,
        });
        self.reasoning_active = false;
        self.reasoning_index = -1;
    }

    /// `finalizeAssistantMessage`
    fn finalize_assistant_message(&mut self, out: &mut String) {
        if !self.message_open {
            return;
        }
        let output_index = self.message_output_index();
        let status = output_status(&self.stop_reason);
        let seq = self.next_seq();
        let text_done = json!({
            "type": "response.output_text.done",
            "sequence_number": seq,
            "item_id": self.current_msg_id,
            "output_index": output_index,
            "content_index": 0,
            "text": self.text,
            "logprobs": [],
        });
        push_event(out, "response.output_text.done", &text_done);
        let seq = self.next_seq();
        let part_done = json!({
            "type": "response.content_part.done",
            "sequence_number": seq,
            "item_id": self.current_msg_id,
            "output_index": output_index,
            "content_index": 0,
            "part": output_text_part(&self.text, &self.annotations),
        });
        push_event(out, "response.content_part.done", &part_done);
        let seq = self.next_seq();
        let item = message_item(&self.current_msg_id, status, &self.text, &self.annotations);
        let item_done = json!({
            "type": "response.output_item.done",
            "sequence_number": seq,
            "output_index": output_index,
            "item": item,
        });
        push_event(out, "response.output_item.done", &item_done);

        self.messages.push(MessageItem {
            id: std::mem::take(&mut self.current_msg_id),
            output_index,
            text: std::mem::take(&mut self.text),
            annotations: std::mem::take(&mut self.annotations),
            status,
        });
        self.in_text_block = false;
        self.message_open = false;
        self.content_part_open = false;
        self.message_output_index = -1;
    }

    /// `finalizeWebSearchWithStatus`: the search's `output_item.done`, once.
    fn finalize_web_search(&mut self, position: usize, status: &'static str, out: &mut String) {
        let search = &mut self.web_searches[position];
        if search.emitted {
            return;
        }
        search.emitted = true;
        search.status = status;
        let output_index = search.output_index;
        let item = search.item(status);
        let seq = self.next_seq();
        let done = json!({
            "type": "response.output_item.done",
            "sequence_number": seq,
            "output_index": output_index,
            "item": item,
        });
        push_event(out, "response.output_item.done", &done);
    }

    /// The block indices of `tool_use` blocks, in ascending order.
    fn tracked_calls(&self) -> Vec<i64> {
        self.funcs
            .iter()
            .filter(|(_, call)| call.tracked)
            .map(|(&index, _)| index)
            .collect()
    }

    fn next_seq(&mut self) -> i64 {
        self.seq += 1;
        self.seq
    }

    /// `allocateOutputIndex`
    fn allocate_output_index(&mut self) -> i64 {
        let index = self.next_output_index;
        self.next_output_index += 1;
        index
    }

    /// `messageOutputIndex`
    fn message_output_index(&mut self) -> i64 {
        if self.message_output_index < 0 {
            self.message_output_index = self.allocate_output_index();
        }
        self.message_output_index
    }

    /// `functionOutputIndex`
    fn function_output_index(&mut self, index: i64) -> i64 {
        self.with_call(index, |this, call| this.call_output_index(call))
    }

    /// `functionOutputIndex` for a call taken out of `funcs`.
    fn call_output_index(&mut self, call: &mut FuncCall) -> i64 {
        if let Some(index) = call.output_index {
            return index;
        }
        let index = self.allocate_output_index();
        call.output_index = Some(index);
        index
    }
}

/// `ConvertClaudeResponseToOpenAIResponsesNonStream`: a whole Claude event
/// stream as one Responses object. Lines up to the first `message_stop` are
/// read. If an `apply_patch` call's arguments are invalid, the result is a
/// failed response that says nothing about them.
pub fn convert_claude_response_to_openai_responses_non_stream(
    original_request: &Value,
    request: &Value,
    response: &[u8],
) -> Value {
    non_stream(original_request, request, response).0
}

/// [`convert_claude_response_to_openai_responses_non_stream`], or `None` if
/// an `apply_patch` call failed: upstream's registry returns nothing then,
/// when its caller passes a parameter to keep the error in.
pub(crate) fn convert_claude_response_to_openai_responses_non_stream_checked(
    original_request: &Value,
    request: &Value,
    response: &[u8],
) -> Option<Value> {
    match non_stream(original_request, request, response) {
        (response, None) => Some(response),
        (_, Some(_)) => None,
    }
}

/// One output item of a complete response.
#[derive(Default)]
struct OutputItem {
    kind: &'static str,
    id: String,
    call_id: String,
    name: String,
    text: String,
    signature: String,
    annotations: Vec<Value>,
    args: String,
    input_snapshot: String,
    results: Option<Value>,
}

/// [`convert_claude_response_to_openai_responses_non_stream`], and the error
/// upstream keeps in its state.
fn non_stream(
    original_request: &Value,
    request: &Value,
    response: &[u8],
) -> (Value, Option<ToolInputError>) {
    let picked = pick_request(original_request, request);
    let tools = RequestTools::new(picked.unwrap_or(&Value::Null));

    let mut response_id = String::new();
    let mut created_at = 0;
    let mut stop_reason = String::new();
    let mut usage = Usage::default();
    let mut items: Vec<OutputItem> = Vec::new();
    let mut block_to_item: HashMap<i64, usize> = HashMap::new();
    let mut web_search_by_tool_id: HashMap<String, usize> = HashMap::new();
    let mut message_count = 0;
    let mut active_message: Option<usize> = None;
    let mut pending_annotations: Vec<Value> = Vec::new();
    let mut identity_conflicts: HashSet<i64> = HashSet::new();
    let mut snapshot_errors: HashMap<i64, InputError> = HashMap::new();

    let fail = |response_id: &str, error: ToolInputError| {
        let mut failed = failure(response_id, 0);
        (failed["response"].take(), Some(error))
    };

    for line in response.split(|&b| b == b'\n') {
        let mut line = line;
        while let [rest @ .., b'\r'] = line {
            line = rest;
        }
        let Some((data, event)) = parse_data_line(line) else {
            if unreadable(line) && patch_enabled(&tools) {
                return fail(&response_id, ToolInputError::Unreadable);
            }
            continue;
        };
        let kind = str_of(event.get("type"));
        // As in the stream, nothing counts after the end of the message.
        if kind == "message_stop" {
            break;
        }
        let index = event.get("index").map_or(0, int_of);
        match &*kind {
            "message_start" => {
                if let Some(message) = event.get("message") {
                    response_id = str_of(message.get("id")).into_owned();
                    created_at = now();
                    usage.merge(message.get("usage"));
                }
            }
            "content_block_start" => {
                let Some(block) = event.get("content_block") else {
                    continue;
                };
                let block_type = str_of(block.get("type"));
                if block_type != "text" {
                    active_message = None;
                }
                match &*block_type {
                    "text" => {
                        let position = match active_message {
                            Some(position) => {
                                block_to_item.insert(index, position);
                                position
                            }
                            None => {
                                let position =
                                    push_item(&mut items, &mut block_to_item, "message", index);
                                items[position].id = format!("msg_{response_id}_{message_count}");
                                message_count += 1;
                                position
                            }
                        };
                        items[position].annotations.append(&mut pending_annotations);
                        active_message = Some(position);
                    }
                    "tool_use" => {
                        let name = str_of(block.get("name"));
                        let call_id = str_of(block.get("id"));
                        let item_type = if tools
                            .winner(tools.identity(&name))
                            .is_some_and(|d| d.kind == "custom")
                        {
                            "custom_tool_call"
                        } else {
                            "function_call"
                        };
                        let position = match block_to_item.get(&index) {
                            Some(&position) => position,
                            None => push_item(&mut items, &mut block_to_item, item_type, index),
                        };
                        let item = &mut items[position];
                        if !call_id.is_empty()
                            && !item.call_id.is_empty()
                            && call_id != item.call_id
                        {
                            identity_conflicts.insert(index);
                        }
                        if (is_apply_patch(&tools, &item.name) || is_apply_patch(&tools, &name))
                            && (identity_conflicts.contains(&index)
                                || (!name.is_empty()
                                    && !item.name.is_empty()
                                    && tools.identity(&name) != tools.identity(&item.name)))
                        {
                            return fail(&response_id, ToolInputError::ConflictingIdentity);
                        }
                        if !name.is_empty() {
                            item.name = name.into_owned();
                            item.kind = item_type;
                        }
                        if !call_id.is_empty() {
                            item.call_id = call_id.into_owned();
                        }
                        if let Some(input) = block.get("input")
                            && !matches!(input, Value::Object(fields) if fields.is_empty())
                        {
                            let raw = raw::member(data, "content_block")
                                .and_then(|block| raw::member(block, "input"))
                                .map_or_else(|| input.to_string(), str::to_owned);
                            if let Err(error) = validate_snapshots(&item.input_snapshot, &raw) {
                                snapshot_errors.entry(index).or_insert(error);
                            }
                            item.input_snapshot = raw;
                        }
                        if is_apply_patch(&tools, &item.name)
                            && let Some(error) = snapshot_errors.get(&index)
                        {
                            return fail(&response_id, ToolInputError::Arguments(error.clone()));
                        }
                        item.id = if item.kind == "custom_tool_call" {
                            format!("ctc_{}", item.call_id)
                        } else {
                            format!("fc_{}", item.call_id)
                        };
                    }
                    "server_tool_use" => {
                        if str_of(block.get("name")) != WEB_SEARCH_TOOL_NAME {
                            continue;
                        }
                        let tool_use_id = str_of(block.get("id")).into_owned();
                        let position =
                            push_item(&mut items, &mut block_to_item, "web_search_call", index);
                        let item = &mut items[position];
                        item.id = responses_web_search_call_id(&tool_use_id);
                        item.call_id.clone_from(&tool_use_id);
                        web_search_by_tool_id.insert(tool_use_id, position);
                        // A stream announces an empty input and fills it in
                        // through deltas; take it only if it has the query.
                        if let Some(input) = block.get("input")
                            && input.is_object()
                        {
                            let raw = raw::member(data, "content_block")
                                .and_then(|block| raw::member(block, "input"))
                                .map_or_else(|| input.to_string(), str::to_owned);
                            if !claude_web_search_query(&raw).is_empty() {
                                item.args.push_str(&raw);
                            }
                        }
                    }
                    "web_search_tool_result" => {
                        let tool_use_id = str_of(block.get("tool_use_id"));
                        if let Some(&position) = web_search_by_tool_id.get(&*tool_use_id) {
                            items[position].results =
                                claude_web_search_results_to_responses(block.get("content"));
                        }
                    }
                    "thinking" | "redacted_thinking" => {
                        let position =
                            push_item(&mut items, &mut block_to_item, "reasoning", index);
                        let item = &mut items[position];
                        item.id = format!("rs_{response_id}_{index}");
                        item.signature = reasoning_carrier(block);
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let Some(delta) = event.get("delta") else {
                    continue;
                };
                let Some(&position) = block_to_item.get(&index) else {
                    // Only a citation can go anywhere but its own block.
                    if str_of(delta.get("type")) == "citations_delta"
                        && let Some(citation) = delta.get("citation")
                    {
                        match active_message {
                            Some(active) => items[active].annotations.push(citation.clone()),
                            None => pending_annotations.push(citation.clone()),
                        }
                    }
                    continue;
                };
                let item = &mut items[position];
                match (&*str_of(delta.get("type")), item.kind) {
                    ("text_delta", "message") => {
                        if let Some(text) = delta.get("text") {
                            item.text.push_str(&str_of(Some(text)));
                        }
                    }
                    (
                        "input_json_delta",
                        "function_call" | "custom_tool_call" | "web_search_call",
                    ) => {
                        if let Some(partial) = delta.get("partial_json") {
                            item.args.push_str(&str_of(Some(partial)));
                        }
                    }
                    ("thinking_delta", "reasoning") => {
                        if let Some(thinking) = delta.get("thinking") {
                            item.text.push_str(&str_of(Some(thinking)));
                        }
                    }
                    ("signature_delta", "reasoning") => {
                        let signature = str_of(delta.get("signature"));
                        if !signature.is_empty() {
                            item.signature = signature.into_owned();
                        }
                    }
                    ("citations_delta", kind) => {
                        if let Some(citation) = delta.get("citation") {
                            if kind == "message" {
                                item.annotations.push(citation.clone());
                            } else if let Some(active) = active_message {
                                items[active].annotations.push(citation.clone());
                            } else {
                                pending_annotations.push(citation.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                usage.merge(event.get("usage"));
                if let Some(reason) = path(&event, "delta.stop_reason") {
                    stop_reason = str_of(Some(reason)).into_owned();
                }
            }
            _ => {}
        }
    }

    let (_, status) = terminal_state(&stop_reason);
    let incomplete = status == "incomplete";
    let mut out = json!({
        "id": response_id,
        "object": "response",
        "created_at": created_at,
        "status": status,
        "background": false,
        "error": null,
        "incomplete_details": if incomplete { incomplete_details() } else { Value::Null },
        "output": [],
        "usage": {
            "input_tokens": 0,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": 0,
            "output_tokens_details": {},
            "total_tokens": 0,
        },
    });
    if let Some(request) = picked {
        for (key, value) in echo_fields(request, |_| None) {
            out[key] = value;
        }
    }

    let mut output = Vec::with_capacity(items.len());
    let last = items.len().saturating_sub(1);
    for (position, item) in items.iter().enumerate() {
        let status = if incomplete && position == last {
            "incomplete"
        } else {
            "completed"
        };
        let value = match item.kind {
            "reasoning" => reasoning_item(&item.id, status, &item.signature, &item.text),
            "web_search_call" => {
                let mut search = build_responses_web_search_call_item(
                    &item.call_id,
                    &claude_web_search_query(&item.args),
                    item.results.as_ref(),
                );
                search.insert("status".into(), status.into());
                Value::Object(search)
            }
            "message" => message_item(&item.id, status, &item.text, &item.annotations),
            "custom_tool_call" => {
                let input = if is_apply_patch(&tools, &item.name) {
                    let mut patch = CallState::default();
                    if let Err(error) = patch.push_arguments(&item.args) {
                        return fail(&response_id, ToolInputError::Arguments(error));
                    }
                    match finish_patch_arguments(&mut patch, &item.args, &item.input_snapshot) {
                        Ok((_, input)) => input,
                        Err(error) => return fail(&response_id, ToolInputError::Arguments(error)),
                    }
                } else {
                    unwrap_custom_tool_input(&item.args)
                };
                call_item(
                    &tools,
                    true,
                    &item.id,
                    status,
                    &input,
                    &item.call_id,
                    &item.name,
                )
            }
            "function_call" => {
                let args = if item.args.is_empty() && status == "completed" {
                    "{}"
                } else {
                    &item.args
                };
                call_item(
                    &tools,
                    false,
                    &item.id,
                    status,
                    args,
                    &item.call_id,
                    &item.name,
                )
            }
            _ => continue,
        };
        output.push(value);
    }
    if !output.is_empty() {
        out["output"] = Value::Array(output);
    }

    let (input, output, total, cached) = usage.responses();
    if input != 0 {
        out["usage"]["input_tokens"] = input.into();
    }
    if cached != 0 {
        out["usage"]["input_tokens_details"]["cached_tokens"] = cached.into();
    }
    if output != 0 {
        out["usage"]["output_tokens"] = output.into();
    }
    if total != 0 {
        out["usage"]["total_tokens"] = total.into();
    }
    let reasoning_length: usize = items
        .iter()
        .filter(|item| item.kind == "reasoning")
        .map(|item| item.text.len())
        .sum();
    let reasoning_tokens = (reasoning_length / 4) as i64;
    if reasoning_tokens > 0 {
        out["usage"]["output_tokens_details"]["reasoning_tokens"] = reasoning_tokens.into();
    }
    (out, None)
}

/// `newOutputItem`: a new item for block `index`, at the end of the output.
fn push_item(
    items: &mut Vec<OutputItem>,
    block_to_item: &mut HashMap<i64, usize>,
    kind: &'static str,
    index: i64,
) -> usize {
    block_to_item.insert(index, items.len());
    items.push(OutputItem {
        kind,
        ..OutputItem::default()
    });
    items.len() - 1
}

/// The token counts Claude reported. A later report replaces the counts it
/// names.
#[derive(Default)]
struct Usage {
    input: i64,
    output: i64,
    cache_creation: i64,
    cache_read: i64,
    /// Whether Claude reported usage at all, even `null`.
    reported: bool,
}

impl Usage {
    /// `Merge`
    fn merge(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage else {
            return;
        };
        self.reported = true;
        if let Some(tokens) = usage.get("input_tokens") {
            self.input = int_of(tokens);
        }
        if let Some(tokens) = usage.get("output_tokens") {
            self.output = int_of(tokens);
        }
        if let Some(tokens) = usage.get("cache_creation_input_tokens") {
            self.cache_creation = int_of(tokens);
        }
        if let Some(tokens) = usage.get("cache_read_input_tokens") {
            self.cache_read = int_of(tokens);
        }
    }

    /// `OpenAIResponsesUsage`: input, output, total and cached tokens. The
    /// input count includes the cached and cache-creation tokens. Sums wrap
    /// as Go's do.
    fn responses(&self) -> (i64, i64, i64, i64) {
        let cached = self.cache_read;
        let input = self
            .input
            .wrapping_add(self.cache_creation)
            .wrapping_add(cached);
        (input, self.output, input.wrapping_add(self.output), cached)
    }
}

/// Splits a line into its `data:` payload and the JSON it holds.
fn parse_data_line(line: &[u8]) -> Option<(&str, Value)> {
    let data = std::str::from_utf8(line.strip_prefix(b"data:")?)
        .ok()?
        .trim();
    Some((data, serde_json::from_str(data).ok()?))
}

/// Whether `line` is a `data:` line [`parse_data_line`] can't read but
/// upstream can: its payload is valid JSON for gjson, though nested too deeply
/// for serde_json, holding an unpaired surrogate escape, or not UTF-8.
/// Bytes that aren't UTF-8 never form JSON's structure, so replacing them
/// keeps it.
fn unreadable(line: &[u8]) -> bool {
    line.strip_prefix(b"data:")
        .is_some_and(|data| raw::valid(String::from_utf8_lossy(data).trim()))
}

/// `isApplyPatch`: whether the request's winning declaration of the tool
/// Claude calls `name` is the `apply_patch` custom tool.
/// Whether the request declares an `apply_patch` tool.
fn patch_enabled(tools: &RequestTools) -> bool {
    tools.winning().any(|d| is_apply_patch(tools, &d.name))
}

fn is_apply_patch(tools: &RequestTools, name: &str) -> bool {
    tools
        .winner(tools.identity(name))
        .is_some_and(|d| d.kind == "custom" && is_custom_tool(&d.tool))
}

/// `validateApplyPatchSnapshots`: equivalent spellings of the arguments are
/// fine, but one complete patch input never replaces another.
fn validate_snapshots(previous: &str, current: &str) -> Result<(), InputError> {
    let mut call = CallState::default();
    if !previous.is_empty() {
        call.finish_arguments(previous)?;
    }
    call.finish_arguments(current).map(drop)
}

/// `finishClaudeApplyPatchArguments`: streamed arguments that are complete
/// JSON are a complete snapshot, not a prefix a later snapshot may extend.
/// Partial arguments can still be completed by a consistent snapshot.
fn finish_patch_arguments(
    call: &mut CallState,
    arguments: &str,
    snapshot: &str,
) -> Result<(String, String), InputError> {
    if snapshot.is_empty() {
        return call.finish_arguments(arguments);
    }
    if raw::valid(arguments) {
        call.finish_arguments(arguments)?;
    }
    call.finish_arguments(snapshot)
}

/// `claudeReasoningCarrier`: the `encrypted_content` for a thinking or
/// redacted thinking block. A streamed thinking block usually starts with an
/// empty signature and gets it from a `signature_delta`.
fn reasoning_carrier(block: &Value) -> String {
    if str_of(block.get("type")) == "redacted_thinking" {
        let data = str_of(block.get("data"));
        if data.is_empty() {
            return String::new();
        }
        return format!("{REDACTED_THINKING_PREFIX}{data}");
    }
    str_of(block.get("signature")).into_owned()
}

/// `claudeResponsesTerminalState`: the final event and the response status.
/// A `max_tokens` stop leaves the response incomplete.
fn terminal_state(stop_reason: &str) -> (&'static str, &'static str) {
    if is_max_tokens(stop_reason) {
        ("response.incomplete", "incomplete")
    } else {
        ("response.completed", "completed")
    }
}

/// `claudeResponsesOutputStatus`
fn output_status(stop_reason: &str) -> &'static str {
    terminal_state(stop_reason).1
}

/// `claudeResponsesIncompleteDetails`
fn incomplete_details() -> Value {
    json!({"reason": "max_output_tokens"})
}

/// `strings.EqualFold(strings.TrimSpace(stop_reason), "max_tokens")`. Go's
/// case folding also matches the Kelvin sign with `k` and the long s with `s`.
fn is_max_tokens(stop_reason: &str) -> bool {
    crate::go::equal_fold(stop_reason.trim(), "max_tokens")
}

/// A reasoning output item with one summary part.
fn reasoning_item(id: &str, status: &str, signature: &str, text: &str) -> Value {
    json!({
        "id": id,
        "type": "reasoning",
        "status": status,
        "encrypted_content": signature,
        "summary": [{"type": "summary_text", "text": text}],
    })
}

/// An `output_text` content part. Citations are written as upstream marshals
/// them.
fn output_text_part(text: &str, annotations: &[Value]) -> Value {
    let annotations = if annotations.is_empty() {
        json!([])
    } else {
        go_value(&Value::Array(annotations.to_vec()))
    };
    json!({
        "type": "output_text",
        "annotations": annotations,
        "logprobs": [],
        "text": text,
    })
}

/// An assistant message output item with one text part.
fn message_item(id: &str, status: &str, text: &str, annotations: &[Value]) -> Value {
    json!({
        "id": id,
        "type": "message",
        "status": status,
        "content": [output_text_part(text, annotations)],
        "role": "assistant",
    })
}

/// A `function_call` item with its `arguments`, or a `custom_tool_call` item
/// with its `input`. `applyResponsesFunctionCallNamespaceFields` turns the
/// Claude name back into the Responses name and namespace.
fn call_item(
    tools: &RequestTools,
    custom: bool,
    id: &str,
    status: &str,
    payload: &str,
    call_id: &str,
    claude_name: &str,
) -> Value {
    let (kind, key) = if custom {
        ("custom_tool_call", "input")
    } else {
        ("function_call", "arguments")
    };
    let (name, namespace) = tools.split(claude_name);
    let mut item = Map::new();
    item.insert("id".into(), id.into());
    item.insert("type".into(), kind.into());
    item.insert("status".into(), status.into());
    item.insert(key.into(), payload.into());
    item.insert("call_id".into(), call_id.into());
    item.insert("name".into(), name.into());
    if !namespace.is_empty() {
        item.insert("namespace".into(), namespace.into());
    }
    Value::Object(item)
}

/// sjson `SetRaw` of `arr.N`: replaces item `index`, or pads the array with
/// `null`s up to it.
fn set_output(output: &mut Vec<Value>, index: i64, item: Value) {
    let Ok(index) = usize::try_from(index) else {
        return;
    };
    if index < output.len() {
        output[index] = item;
    } else {
        output.resize(index, Value::Null);
        output.push(item);
    }
}

fn push_event(out: &mut String, event: &str, data: &Value) {
    out.push_str("event: ");
    out.push_str(event);
    out.push_str("\ndata: ");
    out.push_str(&data.to_string());
    out.push_str("\n\n");
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

#[cfg(test)]
mod tests;
