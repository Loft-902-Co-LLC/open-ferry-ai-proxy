// Ported from CLIProxyAPI internal/translator/openai/openai/responses/openai_openai-responses_response.go
// (ConvertOpenAIChatCompletionsResponseToOpenAIResponses,
// ConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream, FinalizeToolInput)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! OpenAI Chat Completions responses → OpenAI Responses responses.
//!
//! [`OpenAIToOpenAIResponsesStream`] turns each line of a Chat Completions
//! stream into the Responses events it implies, and
//! [`convert_openai_chat_completions_response_to_openai_responses_non_stream`]
//! turns a whole Chat Completions response into one Responses object.
//!
//! A choice's text becomes an assistant message, its reasoning a reasoning
//! item, and each of its tool calls a function call. A call to a tool the
//! request declares as a custom tool becomes a custom tool call instead, its
//! input unwrapped from the `{"input": ...}` arguments. Calls get back the
//! name and namespace the request declared their tool with. Output items are
//! numbered in the order they start. A finish reason closes the open items,
//! but the final event waits for `[DONE]`, so that usage sent after the
//! finish reason still counts. A `length` or `content_filter` finish leaves
//! the response incomplete. A stream that ends with a tool call still open,
//! or with no message or tool call at all, gets no final event.
//!
//! A call to the client's `apply_patch` custom tool streams as custom tool
//! input: the patch text is decoded from the arguments as they arrive.
//! Arguments that aren't one valid input string, a call whose ID or name
//! changes, and a stream that ends before `[DONE]` all end the response with
//! `response.failed`.
//!
//! Deviations from upstream:
//! - A line that is not valid JSON or UTF-8 gives nothing, and a response
//!   body that isn't is read as one with no fields. gjson reads what it can
//!   from malformed JSON.
//! - serde_json can't read JSON nested more than 128 levels deep, holding an
//!   unpaired surrogate escape such as `\ud800` (gjson reads U+FFFD), or a
//!   number too large for `f64`. A line or body that is valid JSON but can't
//!   be read for one of those reasons, or that isn't UTF-8, fails the response
//!   if the request declares `apply_patch`, since it could carry part of a
//!   patch: a stream ends with `response.failed`, and a whole response is the
//!   failed response. Otherwise it is treated as above. Upstream reads it.
//! - A non-string value read as text is written as compact JSON, where
//!   upstream uses its JSON text: content, reasoning, IDs, names and tool
//!   call arguments that are objects or arrays. The `input` in a custom tool
//!   call's arguments is read as upstream reads it. Where a key appears twice
//!   in an object, the last one counts; gjson reads the first.
//! - Strings are written with serde_json's escaping. Upstream writes `<`, `>`,
//!   `&`, U+2028 and U+2029 in some fields as `\u003c` and so on; the JSON
//!   values are the same.
//! - A token count too large for `i64` saturates. Go's result depends on the
//!   CPU; amd64 gives the minimum `i64`.
//! - A `temperature` or `top_p` that reads as infinite or NaN is written as
//!   `null`. Upstream writes `+Inf` or `NaN`, which isn't JSON.
//! - Names cut to 64 bytes start at a character boundary (see
//!   [`super::tools::cap`]).
//! - A client's request that is JSON `null` counts as missing, so the
//!   translated request supplies the tools and the repeated fields. Upstream
//!   takes `null` as the request. The server routes no such request, since it
//!   names no model.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use super::tool_index::ToolNames;
use super::tools::unwrap_custom_tool_input;
use crate::apply_patch::input::{CallState, InputError, failure};
use crate::common::request_model_name;
use crate::common::responses::{echo_fields, pick_request};
use crate::go;
use crate::json::{go_value, int_of, object, path, raw, str_of};

/// Translates a Chat Completions stream into Responses events, one line at a
/// time. Keep one per response.
pub struct OpenAIToOpenAIResponsesStream {
    /// The model `response.created` names: the request's, or the one given
    /// to [`new`](Self::new), or none.
    model: String,
    tools: ToolNames,
    /// The request fields the final event repeats, or `None` without a
    /// request.
    echo: Option<Vec<(&'static str, Value)>>,
    error: Option<ToolInputError>,
    /// Whether any line was given. Upstream's state is made by the first.
    given_line: bool,
    started: bool,
    completed: bool,
    seq: i64,
    response_id: String,
    created_at: i64,
    next_output_index: i64,
    /// The open reasoning item's ID, or `""`.
    reasoning_id: String,
    reasoning_index: i64,
    reasoning_text: String,
    reasonings: Vec<Reasoning>,
    /// Assistant messages, by choice index.
    messages: HashMap<i64, Message>,
    /// Tool calls, by choice index and tool call index.
    calls: HashMap<(i64, i64), Call>,
    finish_reason: String,
    usage: Usage,
}

/// A finished reasoning item.
struct Reasoning {
    id: String,
    text: String,
    output_index: i64,
}

/// One choice's assistant message. It is announced, with its one content
/// part, when its first text arrives.
struct Message {
    output_index: i64,
    text: String,
    /// Whether its done events were sent. Text that arrives later is still
    /// sent as deltas and kept.
    done: bool,
}

/// One tool call. Upstream keeps these fields in `Func*` maps keyed by
/// `"<choice>:<index>"`.
#[derive(Default)]
struct Call {
    output_index: i64,
    call_id: String,
    /// The name the model gave, or once the item is announced or about to
    /// be, its Chat Completions name.
    name: String,
    args: String,
    /// How many bytes of `args` have been sent.
    args_sent: usize,
    /// Whether the model gave the call two different IDs.
    identity_conflict: bool,
    item_added: bool,
    custom: bool,
    item_done: bool,
    /// The input decoded so far, for a call to `apply_patch`.
    patch: Option<CallState>,
}

/// The token counts the stream reported. A later report replaces the counts
/// it names.
#[derive(Default)]
struct Usage {
    prompt: i64,
    cached: i64,
    completion: i64,
    total: i64,
    reasoning: i64,
    seen: bool,
}

impl Usage {
    fn merge(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage else {
            return;
        };
        let mut read = |value: Option<&Value>, count: &mut i64| {
            if let Some(value) = value {
                *count = int_of(value);
                self.seen = true;
            }
        };
        read(usage.get("prompt_tokens"), &mut self.prompt);
        read(
            path(usage, "prompt_tokens_details.cached_tokens"),
            &mut self.cached,
        );
        read(
            usage
                .get("completion_tokens")
                .or_else(|| usage.get("output_tokens")),
            &mut self.completion,
        );
        read(
            path(usage, "output_tokens_details.reasoning_tokens")
                .or_else(|| path(usage, "completion_tokens_details.reasoning_tokens")),
            &mut self.reasoning,
        );
        read(usage.get("total_tokens"), &mut self.total);
    }
}

/// Why a response with an `apply_patch` call failed. Upstream keeps it as an
/// `error` for the caller.
#[derive(Debug)]
enum ToolInputError {
    /// The arguments aren't one valid input string.
    Arguments(InputError),
    /// The call's ID or name changed.
    ConflictingIdentity,
    /// The stream ended before `[DONE]`.
    Unterminated,
    /// A line or body was valid JSON that serde_json can't read. Not
    /// upstream's.
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
            Self::Unreadable => f.write_str("unreadable upstream data in apply_patch response"),
        }
    }
}

impl Error for ToolInputError {}

impl OpenAIToOpenAIResponsesStream {
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
            tools: ToolNames::new(picked.unwrap_or(&Value::Null)),
            echo: picked.map(|request| echo_fields(request, |_| None)),
            error: None,
            given_line: false,
            started: false,
            completed: false,
            seq: 0,
            response_id: String::new(),
            created_at: 0,
            next_output_index: 0,
            reasoning_id: String::new(),
            reasoning_index: 0,
            reasoning_text: String::new(),
            reasonings: Vec::new(),
            messages: HashMap::new(),
            calls: HashMap::new(),
            finish_reason: String::new(),
            usage: Usage::default(),
        }
    }

    /// `ConvertOpenAIChatCompletionsResponseToOpenAIResponses`: translates
    /// one line of the Chat Completions stream, a `data:` line or bare JSON.
    /// Returns the SSE frames to send, `event:` and `data:` lines each, or
    /// `""`. Nothing follows the final event or `response.failed`.
    pub fn translate_line(&mut self, line: &[u8]) -> String {
        let mut out = String::new();
        self.given_line = true;
        if self.error.is_some() || self.completed {
            return out;
        }
        let data = go::trim_space(line.strip_prefix(b"data:").unwrap_or(line));
        if data.is_empty() {
            return out;
        }
        let done = data == b"[DONE]";
        if done && !self.started {
            return out;
        }
        let chunk = if done {
            Value::Null
        } else {
            match serde_json::from_slice::<Value>(data) {
                Ok(chunk) => chunk,
                Err(_) => {
                    if unreadable(data) && self.tools.patch_enabled() {
                        self.fail_tool_input(ToolInputError::Unreadable, &mut out);
                    }
                    return out;
                }
            }
        };
        if !done {
            if let Some(object) = chunk.get("object") {
                let object = str_of(Some(object));
                if !object.is_empty() && object != "chat.completion.chunk" {
                    return out;
                }
            }
            if !chunk.get("choices").is_some_and(Value::is_array) {
                return out;
            }
        }

        self.usage.merge(chunk.get("usage"));
        if !self.started {
            self.start(&chunk, &mut out);
        }
        if done {
            self.done(&mut out);
            return out;
        }
        if let Some(Value::Array(choices)) = chunk.get("choices") {
            for choice in choices {
                if !self.choice(choice, &mut out) {
                    break;
                }
            }
        }
        out
    }

    /// `FinalizeToolInput`: call when the Chat Completions stream ends. If
    /// the request declares `apply_patch` and the stream ended before
    /// `[DONE]` brought the final event, returns `response.failed`, since a
    /// patch may be cut short. Returns `""` if no line was given at all, as
    /// upstream has no state to finalize then.
    pub fn finalize_tool_input(&mut self) -> String {
        let mut out = String::new();
        if self.given_line && !self.completed && self.tools.patch_enabled() {
            self.fail_tool_input(ToolInputError::Unterminated, &mut out);
        }
        out
    }

    /// `ToolInputError`: why the stream failed, if an `apply_patch` call did.
    pub fn tool_input_error(&self) -> Option<&(dyn Error + 'static)> {
        self.error
            .as_ref()
            .map(|error| error as &(dyn Error + 'static))
    }

    /// Starts the response from its first chunk: `response.created` and
    /// `response.in_progress`. Usage the first chunk reported is dropped, as
    /// upstream resets it here after reading it.
    fn start(&mut self, chunk: &Value, out: &mut String) {
        self.response_id = str_of(chunk.get("id")).into_owned();
        self.created_at = chunk.get("created").map_or(0, int_of);
        self.usage = Usage::default();
        self.finish_reason.clear();

        let mut created = Map::new();
        created.insert("id".into(), self.response_id.clone().into());
        created.insert("object".into(), "response".into());
        created.insert("created_at".into(), self.created_at.into());
        created.insert("status".into(), "in_progress".into());
        created.insert("background".into(), false.into());
        created.insert("error".into(), Value::Null);
        created.insert("output".into(), json!([]));
        let mut in_progress = Map::new();
        in_progress.insert("id".into(), self.response_id.clone().into());
        in_progress.insert("object".into(), "response".into());
        in_progress.insert("created_at".into(), self.created_at.into());
        in_progress.insert("status".into(), "in_progress".into());
        in_progress.insert("output".into(), json!([]));
        if !self.model.is_empty() {
            created.insert("model".into(), self.model.clone().into());
            in_progress.insert("model".into(), self.model.clone().into());
        }
        let seq = self.next_seq();
        push_event(
            out,
            "response.created",
            &json!({"type": "response.created", "sequence_number": seq, "response": created}),
        );
        let seq = self.next_seq();
        push_event(
            out,
            "response.in_progress",
            &json!({"type": "response.in_progress", "sequence_number": seq, "response": in_progress}),
        );
        self.started = true;
    }

    /// One choice of a chunk. Returns `false` once the response failed.
    fn choice(&mut self, choice: &Value, out: &mut String) -> bool {
        let index = choice.get("index").map_or(0, int_of);
        if let Some(delta) = choice.get("delta") {
            let mut reasoning = str_of(delta.get("reasoning_content"));
            if reasoning.is_empty() {
                reasoning = str_of(delta.get("reasoning"));
            }
            if !reasoning.is_empty() {
                self.reasoning_delta(index, &reasoning, out);
            }
            let content = str_of(delta.get("content"));
            if !content.is_empty() {
                self.text_delta(index, &content, out);
            }
            if let Some(Value::Array(calls)) = delta.get("tool_calls")
                && !calls.is_empty()
            {
                self.close_reasoning(out);
                // A message open for this choice closes before its calls
                // start, the order Codex expects.
                self.message_done(index, out);
                for call in calls {
                    if !self.tool_call_delta(index, call, out) {
                        break;
                    }
                }
            }
        }
        if self.error.is_some() {
            return false;
        }
        let reason = str_of(choice.get("finish_reason"));
        if !reason.is_empty() {
            self.finish_reason = reason.into_owned();
            self.finalize_open_items(out);
        }
        self.error.is_none()
    }

    fn reasoning_delta(&mut self, index: i64, text: &str, out: &mut String) {
        if self.reasoning_id.is_empty() {
            self.reasoning_id = format!("rs_{}_{index}", self.response_id);
            self.reasoning_index = self.allocate_output_index();
            let seq = self.next_seq();
            push_event(
                out,
                "response.output_item.added",
                &json!({
                    "type": "response.output_item.added",
                    "sequence_number": seq,
                    "output_index": self.reasoning_index,
                    "item": {
                        "id": self.reasoning_id,
                        "type": "reasoning",
                        "status": "in_progress",
                        "summary": [],
                    },
                }),
            );
            let seq = self.next_seq();
            push_event(
                out,
                "response.reasoning_summary_part.added",
                &json!({
                    "type": "response.reasoning_summary_part.added",
                    "sequence_number": seq,
                    "item_id": self.reasoning_id,
                    "output_index": self.reasoning_index,
                    "summary_index": 0,
                    "part": {"type": "summary_text", "text": ""},
                }),
            );
        }
        self.reasoning_text.push_str(text);
        let seq = self.next_seq();
        push_event(
            out,
            "response.reasoning_summary_text.delta",
            &json!({
                "type": "response.reasoning_summary_text.delta",
                "sequence_number": seq,
                "item_id": self.reasoning_id,
                "output_index": self.reasoning_index,
                "summary_index": 0,
                "delta": text,
            }),
        );
    }

    fn text_delta(&mut self, index: i64, text: &str, out: &mut String) {
        self.close_reasoning(out);
        let item_id = format!("msg_{}_{index}", self.response_id);
        if !self.messages.contains_key(&index) {
            let output_index = self.allocate_output_index();
            self.messages.insert(
                index,
                Message {
                    output_index,
                    text: String::new(),
                    done: false,
                },
            );
            let seq = self.next_seq();
            push_event(
                out,
                "response.output_item.added",
                &json!({
                    "type": "response.output_item.added",
                    "sequence_number": seq,
                    "output_index": output_index,
                    "item": {
                        "id": item_id,
                        "type": "message",
                        "status": "in_progress",
                        "content": [],
                        "role": "assistant",
                    },
                }),
            );
            let seq = self.next_seq();
            push_event(
                out,
                "response.content_part.added",
                &json!({
                    "type": "response.content_part.added",
                    "sequence_number": seq,
                    "item_id": item_id,
                    "output_index": output_index,
                    "content_index": 0,
                    "part": {"type": "output_text", "annotations": [], "logprobs": [], "text": ""},
                }),
            );
        }
        let seq = self.next_seq();
        let message = self.messages.get_mut(&index).expect("added above");
        message.text.push_str(text);
        push_event(
            out,
            "response.output_text.delta",
            &json!({
                "type": "response.output_text.delta",
                "sequence_number": seq,
                "item_id": item_id,
                "output_index": message.output_index,
                "content_index": 0,
                "delta": text,
                "logprobs": [],
            }),
        );
    }

    /// One entry of a delta's `tool_calls`. Returns `false` once the response
    /// failed.
    fn tool_call_delta(&mut self, choice: i64, delta: &Value, out: &mut String) -> bool {
        let key = (choice, delta.get("index").map_or(0, int_of));
        if !self.calls.contains_key(&key) {
            let output_index = self.allocate_output_index();
            self.calls.insert(
                key,
                Call {
                    output_index,
                    ..Call::default()
                },
            );
        }
        let new_id = str_of(delta.get("id"));
        let name_chunk = str_of(path(delta, "function.name"));
        let new_name = self.tools.canonical_name(&name_chunk);
        let call = self.calls.get_mut(&key).expect("added above");
        let old_name = self.tools.canonical_name(&call.name);
        // Conflicting IDs are remembered until the winning tool is known.
        if !new_id.is_empty() && !call.call_id.is_empty() && new_id != call.call_id {
            call.identity_conflict = true;
        }
        if (self.tools.is_apply_patch(&old_name) || self.tools.is_apply_patch(&new_name))
            && (call.identity_conflict
                || (!new_name.is_empty() && !old_name.is_empty() && new_name != old_name))
        {
            self.fail_tool_input(ToolInputError::ConflictingIdentity, out);
            return false;
        }
        if !new_id.is_empty() && call.call_id.is_empty() {
            call.call_id = new_id.into_owned();
        }
        if !name_chunk.is_empty() && !call.item_added {
            call.name = name_chunk.into_owned();
        }
        let arguments = str_of(path(delta, "function.arguments"));
        call.args.push_str(&arguments);
        self.emit_tool_item(key, false, out);
        self.emit_pending_args(key, out);
        self.error.is_none()
    }

    /// `emitToolItem`: announces a call once its ID and name are known, or
    /// regardless when `force`d, making up an ID it lacks.
    fn emit_tool_item(&mut self, key: (i64, i64), force: bool, out: &mut String) {
        let call = self.calls.get_mut(&key).expect("a tracked call");
        if call.item_added {
            return;
        }
        call.name = self.tools.canonical_name(&call.name);
        if !force && (call.call_id.is_empty() || call.name.is_empty()) {
            return;
        }
        if call.name.is_empty()
            && let (name, true) = self.tools.single_custom_name()
        {
            call.name = name.to_owned();
        }
        if self.tools.is_apply_patch(&call.name) && call.identity_conflict {
            self.fail_tool_input(ToolInputError::ConflictingIdentity, out);
            return;
        }
        if call.call_id.is_empty() {
            call.call_id = format!("call_{}_{}_{}", self.response_id, key.0, key.1);
        }
        call.custom = self.tools.is_custom(&call.name);
        if call.custom && self.tools.is_apply_patch(&call.name) {
            call.patch = Some(CallState::new(
                format!("ctc_{}", call.call_id),
                call.call_id.clone(),
                call.output_index,
            ));
        }
        let item = call_item(
            &self.tools,
            call.custom,
            &call.call_id,
            "in_progress",
            "",
            &call.name,
        );
        let output_index = call.output_index;
        call.item_added = true;
        let seq = self.next_seq();
        push_event(
            out,
            "response.output_item.added",
            &json!({
                "type": "response.output_item.added",
                "sequence_number": seq,
                "output_index": output_index,
                "item": item,
            }),
        );
    }

    /// `emitPendingFunctionArgs`: sends the arguments that arrived since the
    /// last delta. A custom tool's arguments aren't streamed, except
    /// `apply_patch`'s, which stream as decoded input.
    fn emit_pending_args(&mut self, key: (i64, i64), out: &mut String) {
        let call = self.calls.get_mut(&key).expect("a tracked call");
        if !call.item_added || self.error.is_some() || call.args.len() <= call.args_sent {
            return;
        }
        let delta = &call.args[call.args_sent..];
        if call.custom {
            let Some(patch) = call.patch.as_mut() else {
                return;
            };
            let pushed = patch.push_arguments(delta);
            call.args_sent = call.args.len();
            match pushed {
                Ok(input) if !input.is_empty() => {
                    let seq = next_seq(&mut self.seq);
                    push_event(
                        out,
                        "response.custom_tool_call_input.delta",
                        &patch.input_delta(&input, seq),
                    );
                }
                Ok(_) => {}
                Err(error) => self.fail_tool_input(ToolInputError::Arguments(error), out),
            }
            return;
        }
        let seq = next_seq(&mut self.seq);
        push_event(
            out,
            "response.function_call_arguments.delta",
            &json!({
                "type": "response.function_call_arguments.delta",
                "sequence_number": seq,
                "item_id": format!("fc_{}", call.call_id),
                "output_index": call.output_index,
                "delta": delta,
            }),
        );
        call.args_sent = call.args.len();
    }

    /// `finalizeOpenItems`: closes the open messages, reasoning and tool
    /// calls, each in output order. Without a finish reason, a call whose
    /// arguments are missing or aren't valid JSON is left open, unless it is
    /// to `apply_patch`.
    fn finalize_open_items(&mut self, out: &mut String) {
        if self.error.is_some() {
            return;
        }
        let mut messages: Vec<(i64, i64)> = self
            .messages
            .iter()
            .map(|(&index, message)| (message.output_index, index))
            .collect();
        messages.sort_unstable();
        for (_, index) in messages {
            self.message_done(index, out);
        }
        self.close_reasoning(out);

        // Upstream breaks ties by key, but output indexes are unique.
        let mut calls: Vec<(i64, (i64, i64))> = self
            .calls
            .iter()
            .map(|(&key, call)| (call.output_index, key))
            .collect();
        calls.sort_unstable();
        let incomplete = incomplete_reason(&self.finish_reason).is_some();
        let explicit = matches!(self.finish_reason.as_str(), "tool_calls" | "stop");
        for (_, key) in calls {
            let call = &self.calls[&key];
            if call.item_done {
                continue;
            }
            let has_args = !call.args.is_empty();
            let mut name = self.tools.canonical_name(&call.name);
            if name.is_empty() {
                name = self.tools.single_custom_name().0.to_owned();
            }
            if !self.tools.is_apply_patch(&name)
                && self.finish_reason.is_empty()
                && (!has_args || !raw::valid(&call.args))
            {
                continue;
            }
            self.emit_tool_item(key, true, out);
            self.emit_pending_args(key, out);
            if self.error.is_some() {
                return;
            }
            let call = &self.calls[&key];
            if call.call_id.is_empty() || call.item_done {
                continue;
            }
            let args = if has_args {
                call.args.clone()
            } else if incomplete || !explicit {
                String::new()
            } else {
                "{}".to_owned()
            };
            let status = if incomplete {
                "incomplete"
            } else {
                "completed"
            };
            if !self.finish_call(key, &args, status, out) {
                return;
            }
        }
    }

    /// Sends a call's closing events with its final arguments. Returns
    /// `false` if the response failed.
    fn finish_call(&mut self, key: (i64, i64), args: &str, status: &str, out: &mut String) -> bool {
        let call = self.calls.get_mut(&key).expect("a tracked call");
        let item_id = if call.custom {
            format!("ctc_{}", call.call_id)
        } else {
            format!("fc_{}", call.call_id)
        };
        let payload = if !call.custom {
            let seq = next_seq(&mut self.seq);
            push_event(
                out,
                "response.function_call_arguments.done",
                &json!({
                    "type": "response.function_call_arguments.done",
                    "sequence_number": seq,
                    "item_id": item_id,
                    "output_index": call.output_index,
                    "arguments": args,
                }),
            );
            args.to_owned()
        } else if let Some(patch) = call.patch.as_mut() {
            let (tail, input) = match patch.finish_arguments(args) {
                Ok(finished) => finished,
                Err(error) => {
                    self.fail_tool_input(ToolInputError::Arguments(error), out);
                    return false;
                }
            };
            if !tail.is_empty() {
                let seq = next_seq(&mut self.seq);
                push_event(
                    out,
                    "response.custom_tool_call_input.delta",
                    &patch.input_delta(&tail, seq),
                );
            }
            let seq = next_seq(&mut self.seq);
            push_event(
                out,
                "response.custom_tool_call_input.done",
                &patch.input_done(&input, seq),
            );
            input
        } else {
            let input = unwrap_custom_tool_input(args);
            let seq = next_seq(&mut self.seq);
            push_event(
                out,
                "response.custom_tool_call_input.done",
                &json!({
                    "type": "response.custom_tool_call_input.done",
                    "sequence_number": seq,
                    "item_id": item_id,
                    "output_index": call.output_index,
                    "input": input,
                }),
            );
            input
        };
        let item = call_item(
            &self.tools,
            call.custom,
            &call.call_id,
            status,
            &payload,
            &call.name,
        );
        call.item_done = true;
        let output_index = call.output_index;
        let seq = self.next_seq();
        push_event(
            out,
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "sequence_number": seq,
                "output_index": output_index,
                "item": item,
            }),
        );
        true
    }

    /// `emitMessageItemDone`: closes the message of choice `index`, if it is
    /// open.
    fn message_done(&mut self, index: i64, out: &mut String) {
        let status = output_status(&self.finish_reason);
        let item_id = format!("msg_{}_{index}", self.response_id);
        let Some(message) = self.messages.get_mut(&index) else {
            return;
        };
        if message.done {
            return;
        }
        message.done = true;
        let output_index = message.output_index;
        let text = message.text.clone();
        let seq = self.next_seq();
        push_event(
            out,
            "response.output_text.done",
            &json!({
                "type": "response.output_text.done",
                "sequence_number": seq,
                "item_id": item_id,
                "output_index": output_index,
                "content_index": 0,
                "text": text,
                "logprobs": [],
            }),
        );
        let seq = self.next_seq();
        push_event(
            out,
            "response.content_part.done",
            &json!({
                "type": "response.content_part.done",
                "sequence_number": seq,
                "item_id": item_id,
                "output_index": output_index,
                "content_index": 0,
                "part": output_text_part(&text),
            }),
        );
        let seq = self.next_seq();
        push_event(
            out,
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "sequence_number": seq,
                "output_index": output_index,
                "item": message_item(&item_id, status, &text),
            }),
        );
    }

    /// `stopReasoning`: closes the open reasoning item, if there is one.
    fn close_reasoning(&mut self, out: &mut String) {
        if self.reasoning_id.is_empty() {
            return;
        }
        let id = mem::take(&mut self.reasoning_id);
        let text = mem::take(&mut self.reasoning_text);
        let output_index = self.reasoning_index;
        let seq = self.next_seq();
        push_event(
            out,
            "response.reasoning_summary_text.done",
            &json!({
                "type": "response.reasoning_summary_text.done",
                "sequence_number": seq,
                "item_id": id,
                "output_index": output_index,
                "summary_index": 0,
                "text": text,
            }),
        );
        let seq = self.next_seq();
        push_event(
            out,
            "response.reasoning_summary_part.done",
            &json!({
                "type": "response.reasoning_summary_part.done",
                "sequence_number": seq,
                "item_id": id,
                "output_index": output_index,
                "summary_index": 0,
                "part": {"type": "summary_text", "text": text},
            }),
        );
        let seq = self.next_seq();
        push_event(
            out,
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "item": {
                    "id": id,
                    "type": "reasoning",
                    "encrypted_content": "",
                    "summary": [{"type": "summary_text", "text": text}],
                },
                "output_index": output_index,
                "sequence_number": seq,
            }),
        );
        self.reasonings.push(Reasoning {
            id,
            text,
            output_index,
        });
    }

    /// `[DONE]`: closes what is open and, unless a tool call is still open or
    /// nothing but reasoning came, sends the final event.
    fn done(&mut self, out: &mut String) {
        self.finalize_open_items(out);
        if self.error.is_some() {
            return;
        }
        if self
            .calls
            .values()
            .any(|call| call.item_added && !call.item_done)
        {
            return;
        }
        if self.messages.is_empty() && !self.calls.values().any(|call| call.item_added) {
            return;
        }
        self.completed = true;
        let (event, data) = self.completed_event();
        push_event(out, event, &data);
    }

    /// `buildResponsesCompletedEvent`: `response.completed`, or
    /// `response.incomplete`, with every finished item.
    fn completed_event(&mut self) -> (&'static str, Value) {
        let reason = incomplete_reason(&self.finish_reason);
        let (event, status) = match reason {
            Some(_) => ("response.incomplete", "incomplete"),
            None => ("response.completed", "completed"),
        };
        let seq = self.next_seq();
        let mut response = Map::new();
        response.insert("id".into(), self.response_id.clone().into());
        response.insert("object".into(), "response".into());
        response.insert("created_at".into(), self.created_at.into());
        response.insert("status".into(), status.into());
        response.insert("background".into(), false.into());
        response.insert("error".into(), Value::Null);
        if let Some(reason) = reason {
            response.insert("incomplete_details".into(), json!({"reason": reason}));
        }
        for (key, value) in self.echo.iter().flatten() {
            response.insert((*key).into(), value.clone());
        }

        let mut output: Vec<(i64, Value)> = Vec::new();
        for reasoning in &self.reasonings {
            output.push((
                reasoning.output_index,
                json!({
                    "id": reasoning.id,
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": reasoning.text}],
                }),
            ));
        }
        for (index, message) in &self.messages {
            let id = format!("msg_{}_{index}", self.response_id);
            output.push((
                message.output_index,
                message_item(&id, status, &message.text),
            ));
        }
        for call in self.calls.values().filter(|call| call.item_done) {
            let payload = match (&call.patch, call.custom) {
                (Some(patch), _) => patch.input().to_owned(),
                (None, true) => unwrap_custom_tool_input(&call.args),
                (None, false) => call.args.clone(),
            };
            let item = call_item(
                &self.tools,
                call.custom,
                &call.call_id,
                status,
                &payload,
                &call.name,
            );
            output.push((call.output_index, item));
        }
        if !output.is_empty() {
            output.sort_by_key(|(index, _)| *index);
            let output = output.into_iter().map(|(_, item)| item).collect();
            response.insert("output".into(), Value::Array(output));
        }

        if self.usage.seen {
            let usage = &self.usage;
            let mut counts = Map::new();
            counts.insert("input_tokens".into(), usage.prompt.into());
            counts.insert(
                "input_tokens_details".into(),
                json!({"cached_tokens": usage.cached}),
            );
            counts.insert("output_tokens".into(), usage.completion.into());
            if usage.reasoning > 0 {
                counts.insert(
                    "output_tokens_details".into(),
                    json!({"reasoning_tokens": usage.reasoning}),
                );
            }
            let total = match usage.total {
                0 => usage.prompt.wrapping_add(usage.completion),
                total => total,
            };
            counts.insert("total_tokens".into(), total.into());
            response.insert("usage".into(), Value::Object(counts));
        }
        // Not `json!`, which re-reads `response` and respells its numbers.
        let response = object([
            ("type", event.into()),
            ("sequence_number", seq.into()),
            ("response", Value::Object(response)),
        ]);
        (event, response)
    }

    fn fail_tool_input(&mut self, error: ToolInputError, out: &mut String) {
        if self.error.is_some() {
            return;
        }
        self.error = Some(error);
        let seq = self.next_seq();
        push_event(out, "response.failed", &failure(&self.response_id, seq));
    }

    fn next_seq(&mut self) -> i64 {
        next_seq(&mut self.seq)
    }

    fn allocate_output_index(&mut self) -> i64 {
        let index = self.next_output_index;
        self.next_output_index += 1;
        index
    }
}

/// `ConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream`: a whole
/// Chat Completions response as one Responses object. A response without an
/// `id` gets a new one, and one without `created` the current time. If an
/// `apply_patch` call's arguments are invalid, the result is a failed
/// response that says nothing about them.
pub fn convert_openai_chat_completions_response_to_openai_responses_non_stream(
    original_request: &Value,
    request: &Value,
    response: &[u8],
) -> Value {
    non_stream(original_request, request, response).0
}

/// [`convert_openai_chat_completions_response_to_openai_responses_non_stream`],
/// or `None` if an `apply_patch` call failed: upstream's registry returns
/// nothing then, when its caller passes a parameter to keep the error in.
pub(crate) fn convert_openai_chat_completions_response_to_openai_responses_non_stream_checked(
    original_request: &Value,
    request: &Value,
    response: &[u8],
) -> Option<Value> {
    match non_stream(original_request, request, response) {
        (response, None) => Some(response),
        (_, Some(_)) => None,
    }
}

/// [`convert_openai_chat_completions_response_to_openai_responses_non_stream`],
/// and the error upstream keeps in its state. The tools come from the
/// client's request, or else the translated one; the repeated fields come
/// from the translated request only, as upstream's do.
fn non_stream(
    original_request: &Value,
    request: &Value,
    body: &[u8],
) -> (Value, Option<ToolInputError>) {
    let tools = ToolNames::new(pick_request(original_request, request).unwrap_or(&Value::Null));
    let root = match serde_json::from_slice::<Value>(body) {
        Ok(root) => root,
        Err(_) if unreadable(body) && tools.patch_enabled() => {
            let mut failed = failure(&new_response_id(), 0);
            return (failed["response"].take(), Some(ToolInputError::Unreadable));
        }
        Err(_) => Value::Null,
    };
    let first = match root.get("choices") {
        Some(Value::Array(choices)) => choices.first(),
        Some(choices @ Value::Object(_)) => choices.get("0"),
        _ => None,
    };
    let reason = incomplete_reason(&str_of(
        first.and_then(|choice| choice.get("finish_reason")),
    ));
    let status = match reason {
        Some(_) => "incomplete",
        None => "completed",
    };

    let id = match str_of(root.get("id")) {
        id if id.is_empty() => new_response_id(),
        id => id.into_owned(),
    };
    let created_at = match root.get("created").map_or(0, int_of) {
        0 => now(),
        created_at => created_at,
    };
    let mut out = Map::new();
    out.insert("id".into(), id.clone().into());
    out.insert("object".into(), "response".into());
    out.insert("created_at".into(), created_at.into());
    out.insert("status".into(), status.into());
    out.insert("background".into(), false.into());
    out.insert("error".into(), Value::Null);
    out.insert(
        "incomplete_details".into(),
        reason.map_or(Value::Null, |reason| json!({"reason": reason})),
    );
    if !request.is_null() {
        // A Chat Completions `max_tokens` stands in for `max_output_tokens`,
        // and the response's model for the request's.
        let fallback = |key: &str| match key {
            "max_output_tokens" => request.get("max_tokens"),
            "model" => root.get("model"),
            _ => None,
        };
        for (key, value) in echo_fields(request, fallback) {
            out.insert(key.into(), value);
        }
    } else if let Some(model) = root.get("model") {
        out.insert("model".into(), str_of(Some(model)).into_owned().into());
    }

    let mut output = Vec::new();
    let message = first.and_then(|choice| choice.get("message"));
    let mut reasoning = str_of(message.and_then(|message| message.get("reasoning_content")));
    if reasoning.is_empty() {
        reasoning = str_of(message.and_then(|message| message.get("reasoning")));
    }
    if !reasoning.is_empty() || request.get("reasoning").is_some() {
        let summary = if reasoning.is_empty() {
            json!([])
        } else {
            json!([{"type": "summary_text", "text": reasoning}])
        };
        output.push(json!({
            "id": format!("rs_{}", id.strip_prefix("resp_").unwrap_or(&id)),
            "type": "reasoning",
            "encrypted_content": "",
            "summary": summary,
        }));
    }

    for choice in root
        .get("choices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(message) = choice.get("message") else {
            continue;
        };
        let index = choice.get("index").map_or(0, int_of);
        let content = str_of(message.get("content"));
        if !content.is_empty() {
            output.push(message_item(&format!("msg_{id}_{index}"), status, &content));
        }
        let Some(Value::Array(calls)) = message.get("tool_calls") else {
            continue;
        };
        for (position, call) in calls.iter().enumerate() {
            // Providers may leave out a call's ID; one is made up so the
            // client can answer the call.
            let call_id = match str_of(call.get("id")) {
                call_id if call_id.is_empty() => format!("call_{id}_{index}_{position}"),
                call_id => call_id.into_owned(),
            };
            let name = tools.canonical_name(&str_of(path(call, "function.name")));
            let args = str_of(path(call, "function.arguments"));
            if !tools.is_custom(&name) {
                output.push(call_item(&tools, false, &call_id, status, &args, &name));
                continue;
            }
            let input = if tools.is_apply_patch(&name) {
                match CallState::default().finish_arguments(&*args) {
                    Ok((_, input)) => input,
                    Err(error) => {
                        let mut failed = failure(&id, 0);
                        return (
                            failed["response"].take(),
                            Some(ToolInputError::Arguments(error)),
                        );
                    }
                }
            } else {
                unwrap_custom_tool_input(&args)
            };
            output.push(call_item(&tools, true, &call_id, status, &input, &name));
        }
    }
    if !output.is_empty() {
        out.insert("output".into(), Value::Array(output));
    }

    if let Some(usage) = root.get("usage") {
        let counted = ["prompt_tokens", "completion_tokens", "total_tokens"]
            .iter()
            .any(|key| usage.get(key).is_some());
        if counted {
            let count = |key: &str| usage.get(key).map_or(0, int_of);
            let mut counts = Map::new();
            counts.insert("input_tokens".into(), count("prompt_tokens").into());
            if let Some(cached) = path(usage, "prompt_tokens_details.cached_tokens") {
                counts.insert(
                    "input_tokens_details".into(),
                    json!({"cached_tokens": int_of(cached)}),
                );
            }
            counts.insert("output_tokens".into(), count("completion_tokens").into());
            if let Some(reasoning) = path(usage, "output_tokens_details.reasoning_tokens") {
                counts.insert(
                    "output_tokens_details".into(),
                    json!({"reasoning_tokens": int_of(reasoning)}),
                );
            }
            counts.insert("total_tokens".into(), count("total_tokens").into());
            out.insert("usage".into(), Value::Object(counts));
        } else {
            out.insert("usage".into(), go_value(usage));
        }
    }
    (Value::Object(out), None)
}

/// `incompleteByFinishReason`: why a response that finished for `reason` is
/// incomplete, if it is.
fn incomplete_reason(reason: &str) -> Option<&'static str> {
    match reason {
        "length" | "max_tokens" => Some("max_output_tokens"),
        "content_filter" => Some("content_filter"),
        _ => None,
    }
}

/// The status of an item that finished for `reason`.
fn output_status(reason: &str) -> &'static str {
    match incomplete_reason(reason) {
        Some(_) => "incomplete",
        None => "completed",
    }
}

/// Whether `data`, which serde_json can't read, is valid JSON to gjson:
/// nested too deeply for serde_json, holding an unpaired surrogate escape or
/// a number too large for `f64`, or not UTF-8. Bytes that aren't UTF-8 never
/// form JSON's structure, so replacing them keeps it.
fn unreadable(data: &[u8]) -> bool {
    raw::valid(String::from_utf8_lossy(data).trim())
}

/// An `output_text` content part.
fn output_text_part(text: &str) -> Value {
    json!({"type": "output_text", "annotations": [], "logprobs": [], "text": text})
}

/// An assistant message output item with one text part.
fn message_item(id: &str, status: &str, text: &str) -> Value {
    json!({
        "id": id,
        "type": "message",
        "status": status,
        "content": [output_text_part(text)],
        "role": "assistant",
    })
}

/// A `function_call` item with its `arguments`, or a `custom_tool_call` item
/// with its `input`. `applyIdentity` turns the Chat Completions name back
/// into the Responses name and namespace.
fn call_item(
    tools: &ToolNames,
    custom: bool,
    call_id: &str,
    status: &str,
    payload: &str,
    chat_name: &str,
) -> Value {
    let (prefix, kind, key) = if custom {
        ("ctc_", "custom_tool_call", "input")
    } else {
        ("fc_", "function_call", "arguments")
    };
    let (name, namespace) = tools.identity(chat_name);
    let mut item = Map::new();
    item.insert("id".into(), format!("{prefix}{call_id}").into());
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

fn next_seq(seq: &mut i64) -> i64 {
    *seq += 1;
    *seq
}

fn push_event(out: &mut String, event: &str, data: &Value) {
    out.push_str("event: ");
    out.push_str(event);
    out.push_str("\ndata: ");
    out.push_str(&data.to_string());
    out.push_str("\n\n");
}

/// Counts the response IDs made up, as upstream's `responseIDCounter` does.
static RESPONSE_IDS: AtomicU64 = AtomicU64::new(0);

/// A response ID for a response that has none: the time in nanoseconds, in
/// hex, and a count.
fn new_response_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as i64);
    let count = RESPONSE_IDS.fetch_add(1, Ordering::Relaxed) + 1;
    format!("resp_{nanos:x}_{count}")
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

#[cfg(test)]
mod tests;
