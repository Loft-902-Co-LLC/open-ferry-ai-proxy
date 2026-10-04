// Ported from CLIProxyAPI internal/translator/openai/interactions/responses/interactions_openai_responses_response.go
// (ConvertOpenAIResponsesResponseToInteractions,
// ConvertOpenAIResponsesResponseToInteractionsNonStream,
// convertOpenAIResponsesEventToInteractions,
// openAIResponsesOutputItemToInteractionsStep,
// openAIResponsesOutputItemAddedToInteractions,
// openAIResponsesOutputItemDoneToInteractions,
// openAIResponsesCompletedToInteractions,
// appendResponsesMessageFallbackToInteractions, responseOutputIndexRoot,
// appendInteractionsCreatedDirect, appendInteractionsStatusUpdateDirect,
// ensureInteractionsStepDirect, ensureInteractionsCreatedDirect,
// appendInteractionsStepStartDirect, appendInteractionsTextDeltaDirect,
// appendInteractionsArgumentsDeltaDirect, appendInteractionsStepStopDirect,
// appendInteractionsCompletedDirect, appendInteractionsDoneDirect,
// ensureInteractionsFunctionCallStep, setInteractionsUsageFromResponses,
// textKeysFromResponsesEvent, functionArgsKeysFromResponsesEvent,
// openAIResponsesTextKeys, openAIResponsesUnkeyedTextKeys, markTextSent,
// hasSentText, hasSentUnkeyedText, markFunctionArgsSent,
// hasSentFunctionArgs), and Go's time.Time.Format with the RFC 3339 layout
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! OpenAI Responses responses → Interactions events and responses.
//!
//! Each Responses output item becomes a step: a message a `model_output`
//! step, a reasoning item a `thought` step and a function call a
//! `function_call` step, its namespace joined to its name. A stream opens
//! one step at a time, closing the open one when another kind starts. Text
//! a message's `output_item.done` or the completed response repeats is sent
//! only if its deltas weren't.
//!
//! Deviations from upstream: see the [module](super)'s.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::items::{
    content_part_to_interactions, first_non_empty, for_each, function_call_to_interactions, get,
    json_string_value, key_int, response_model, set, text,
};
use super::read::{read, sse_payload};
use crate::common::sse::{push_event, push_frame};
use crate::go;
use crate::json::int_of;

/// `ConvertOpenAIResponsesResponseToInteractions`: turns a Responses event
/// stream into Interactions events. Make one per response.
pub struct OpenAIResponsesToInteractionsStream {
    model: String,
    id: String,
    created: bool,
    status_updated: bool,
    completed: bool,
    done: bool,
    /// The index the next step gets.
    step_index: i64,
    active_step_index: i64,
    active_step_type: String,
    active_step_open: bool,
    /// The keys of the text parts whose deltas were sent.
    sent_text: HashSet<String>,
    /// Whether a text delta without a content index was sent.
    unkeyed_text_delta: bool,
    /// The keys of the calls whose argument deltas were sent.
    function_args_sent: HashSet<String>,
}

impl OpenAIResponsesToInteractionsStream {
    /// A stream for a response from `model`.
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            id: String::new(),
            created: false,
            status_updated: false,
            completed: false,
            done: false,
            step_index: 0,
            active_step_index: 0,
            active_step_type: String::new(),
            active_step_open: false,
            sent_text: HashSet::new(),
            unkeyed_text_delta: false,
            function_args_sent: HashSet::new(),
        }
    }

    /// Translates one chunk of the Responses stream, an SSE frame or a
    /// `data:` line, into the Interactions events it gives, as SSE text.
    pub fn translate(&mut self, chunk: &[u8]) -> String {
        let mut out = String::new();
        let payload = sse_payload(chunk);
        if payload.is_empty() {
            return out;
        }
        if go::trim_space(&payload) == b"[DONE]" {
            self.done_event(&mut out);
            return out;
        }
        let Some(root) = read(&payload).0 else {
            return out;
        };
        let root = &root;
        match text(Some(root), "type").as_ref() {
            "response.created" => {
                self.created_event(&mut out, root.get("response"));
            }
            "response.output_text.delta" => {
                self.ensure_step(&mut out, "model_output");
                self.text_delta(&mut out, &text(Some(root), "delta"), false);
                self.mark_text_sent(text_keys_from_event(root));
            }
            "response.reasoning_summary_text.delta" => {
                self.ensure_step(&mut out, "thought");
                self.text_delta(&mut out, &text(Some(root), "delta"), true);
            }
            "response.output_item.added" => self.item_added(&mut out, root),
            "response.function_call_arguments.delta" => {
                self.ensure_function_call_step(&mut out, root);
                self.arguments_delta(&mut out, &text(Some(root), "delta"));
                self.function_args_sent
                    .extend(function_args_keys_from_event(root));
            }
            "response.output_item.done" => self.item_done(&mut out, root),
            "response.completed" | "response.incomplete" => {
                self.response_completed(&mut out, root.get("response"));
            }
            _ => {}
        }
        out
    }

    /// `openAIResponsesOutputItemAddedToInteractions`
    fn item_added(&mut self, out: &mut String, root: &Value) {
        let item = root.get("item");
        match text(item, "type").as_ref() {
            "function_call" => {
                self.created_event(out, None);
                self.step_stop(out);
                let mut step = json!({ "type": "function_call", "name": "", "arguments": {} });
                set(&mut step, "name", text(item, "name"));
                let (own, id) = (text(item, "call_id"), text(item, "id"));
                let call_id = first_non_empty([&own, &id]);
                if !call_id.is_empty() {
                    set(&mut step, "id", call_id);
                    set(&mut step, "call_id", call_id);
                }
                self.step_start(out, "function_call", Some(&step));
            }
            "message" => self.ensure_step(out, "model_output"),
            "reasoning" => self.ensure_step(out, "thought"),
            _ => {}
        }
    }

    /// `openAIResponsesOutputItemDoneToInteractions`
    fn item_done(&mut self, out: &mut String, root: &Value) {
        let item = root.get("item");
        match text(item, "type").as_ref() {
            "function_call" => {
                self.ensure_function_call_step(out, root);
                let arguments = get(item, "arguments");
                let sent = function_args_keys_from_event(root)
                    .iter()
                    .any(|key| self.function_args_sent.contains(key));
                if arguments.is_some() && !text(item, "arguments").is_empty() && !sent {
                    self.arguments_delta(out, &json_string_value(arguments, "{}"));
                }
                self.step_stop(out);
            }
            "reasoning" => {
                self.ensure_step(out, "thought");
                for (_, summary) in for_each(get(item, "summary")) {
                    let summary = text(Some(summary), "text");
                    if !summary.is_empty() {
                        self.text_delta(out, &summary, true);
                    }
                }
                self.step_stop(out);
            }
            "message" => {
                if let Some(item) = item {
                    self.message_fallback(out, item, root, true);
                }
            }
            _ => {}
        }
    }

    /// `openAIResponsesCompletedToInteractions`: the text the deltas didn't
    /// send, then the open step's stop, `interaction.completed` and `done`.
    fn response_completed(&mut self, out: &mut String, response: Option<&Value>) {
        for (output_index, item) in for_each(get(response, "output")) {
            if text(Some(item), "type") == "message" {
                let mut root = json!({ "output_index": key_int(output_index.as_ref()) });
                let id = text(Some(item), "id");
                if !id.is_empty() {
                    set(&mut root, "item_id", id);
                }
                self.message_fallback(out, item, &root, false);
            }
        }
        self.step_stop(out);
        self.completed_event(out, response);
        self.done_event(out);
    }

    /// `appendResponsesMessageFallbackToInteractions`: the text parts of a
    /// finished message whose deltas weren't sent, then, if `stop`, the open
    /// step's stop.
    fn message_fallback(&mut self, out: &mut String, item: &Value, root: &Value, stop: bool) {
        let item_id = text(Some(item), "id");
        let output_index = root.get("output_index");
        let output = output_index.map(int_of);
        for (content_index, part) in for_each(item.get("content")) {
            let part_type = text(Some(part), "type");
            if part_type != "output_text" && part_type != "text" {
                continue;
            }
            let content = content_index.as_ref().map(int_of);
            let keys = text_keys(&item_id, output, content);
            let unkeyed = unkeyed_text_keys(&item_id, output);
            if self.has_sent_text(&keys, content.is_some()) || self.has_sent_unkeyed_text(&unkeyed)
            {
                continue;
            }
            let part_text = text(Some(part), "text");
            if part_text.is_empty() {
                continue;
            }
            self.ensure_step(out, "model_output");
            self.text_delta(out, &part_text, false);
            self.mark_text_sent(keys);
        }
        if stop {
            self.step_stop(out);
        }
    }

    /// `appendInteractionsCreatedDirect`: `interaction.created`, then
    /// `interaction.status_update`, once.
    fn created_event(&mut self, out: &mut String, response: Option<&Value>) {
        if self.created {
            return;
        }
        let response_id = text(response, "id");
        let generated = format!("interaction_{}", unix_nanos());
        self.id = first_non_empty([&response_id, &self.id, &generated]).to_owned();
        let mut created = json!({
            "interaction": {
                "id": "",
                "status": "in_progress",
                "object": "interaction",
                "model": "",
            },
            "event_type": "interaction.created",
        });
        set(&mut created, "interaction.id", self.id.as_str());
        set(
            &mut created,
            "interaction.model",
            response_model(&self.model, response),
        );
        push_event(out, "interaction.created", &created);
        self.created = true;
        if !self.status_updated {
            let mut update = json!({
                "interaction_id": "",
                "status": "in_progress",
                "event_type": "interaction.status_update",
            });
            set(&mut update, "interaction_id", self.id.as_str());
            push_event(out, "interaction.status_update", &update);
            self.status_updated = true;
        }
    }

    /// `ensureInteractionsStepDirect`: a step of `step_type` open, opening
    /// one if the open step is of another type.
    fn ensure_step(&mut self, out: &mut String, step_type: &str) {
        self.created_event(out, None);
        if self.active_step_open && self.active_step_type == step_type {
            return;
        }
        self.step_stop(out);
        self.step_start(out, step_type, None);
    }

    /// `ensureInteractionsFunctionCallStep`: a function call step open, for
    /// the call the event is about.
    fn ensure_function_call_step(&mut self, out: &mut String, root: &Value) {
        if self.active_step_open && self.active_step_type == "function_call" {
            return;
        }
        let item = root.get("item").unwrap_or(root);
        let mut step = json!({ "type": "function_call", "name": "", "arguments": {} });
        set(&mut step, "name", text(Some(item), "name"));
        let ids = [
            text(Some(item), "call_id"),
            text(Some(item), "id"),
            text(Some(root), "call_id"),
            text(Some(root), "item_id"),
        ];
        let call_id = first_non_empty([&ids[0], &ids[1], &ids[2], &ids[3]]);
        if !call_id.is_empty() {
            set(&mut step, "id", call_id);
            set(&mut step, "call_id", call_id);
        }
        self.created_event(out, None);
        self.step_stop(out);
        self.step_start(out, "function_call", Some(&step));
    }

    /// `appendInteractionsStepStartDirect`
    fn step_start(&mut self, out: &mut String, step_type: &str, step: Option<&Value>) {
        let index = self.step_index;
        self.step_index += 1;
        self.active_step_index = index;
        self.active_step_type = step_type.to_owned();
        self.active_step_open = true;
        let mut payload = json!({ "index": 0, "step": { "type": "" }, "event_type": "step.start" });
        set(&mut payload, "index", index);
        set(&mut payload, "step.type", step_type);
        if step_type == "function_call" {
            let (call_id, id) = (text(step, "call_id"), text(step, "id"));
            let id = first_non_empty([&call_id, &id]);
            if !id.is_empty() {
                set(&mut payload, "step.id", id);
                set(&mut payload, "step.call_id", id);
            }
            set(&mut payload, "step.name", text(step, "name"));
            set(&mut payload, "step.arguments", json!({}));
        }
        push_event(out, "step.start", &payload);
    }

    /// `appendInteractionsTextDeltaDirect`: a text delta, or for a `thought`
    /// a summary delta, to the open step.
    fn text_delta(&mut self, out: &mut String, text: &str, thought: bool) {
        let mut payload = if thought {
            let mut payload = json!({
                "index": 0,
                "delta": { "content": { "text": "", "type": "text" }, "type": "thought_summary" },
                "event_type": "step.delta",
            });
            set(&mut payload, "delta.content.text", text);
            payload
        } else {
            let mut payload = json!({
                "index": 0,
                "delta": { "text": "", "type": "text" },
                "event_type": "step.delta",
            });
            set(&mut payload, "delta.text", text);
            payload
        };
        set(&mut payload, "index", self.active_step_index);
        push_event(out, "step.delta", &payload);
    }

    /// `appendInteractionsArgumentsDeltaDirect`
    fn arguments_delta(&mut self, out: &mut String, arguments: &str) {
        let mut payload = json!({
            "index": 0,
            "delta": { "arguments": "", "type": "arguments_delta" },
            "event_type": "step.delta",
        });
        set(&mut payload, "index", self.active_step_index);
        set(&mut payload, "delta.arguments", arguments);
        push_event(out, "step.delta", &payload);
    }

    /// `appendInteractionsStepStopDirect`: the open step's stop, if one is
    /// open.
    fn step_stop(&mut self, out: &mut String) {
        if !self.active_step_open {
            return;
        }
        let mut payload = json!({ "index": 0, "event_type": "step.stop" });
        set(&mut payload, "index", self.active_step_index);
        push_event(out, "step.stop", &payload);
        self.active_step_open = false;
        self.active_step_type.clear();
    }

    /// `appendInteractionsCompletedDirect`: `interaction.completed`, once.
    fn completed_event(&mut self, out: &mut String, response: Option<&Value>) {
        if self.completed {
            return;
        }
        let now = rfc3339(unix_seconds());
        let mut payload = json!({
            "interaction": {
                "id": "",
                "status": "completed",
                "usage": {},
                "created": "",
                "updated": "",
                "service_tier": "standard",
                "object": "interaction",
                "model": "",
            },
            "event_type": "interaction.completed",
        });
        set(&mut payload, "interaction.id", self.id.as_str());
        set(&mut payload, "interaction.created", now.as_str());
        set(&mut payload, "interaction.updated", now);
        set(
            &mut payload,
            "interaction.model",
            response_model(&self.model, response),
        );
        let status = text(response, "status");
        if !status.is_empty() {
            set(&mut payload, "interaction.status", status);
        }
        set_usage(&mut payload, "interaction.usage", get(response, "usage"));
        push_event(out, "interaction.completed", &payload);
        self.completed = true;
    }

    /// `appendInteractionsDoneDirect`: `done`, once.
    fn done_event(&mut self, out: &mut String) {
        if self.done {
            return;
        }
        push_frame(out, "done", "[DONE]");
        self.done = true;
    }

    /// `markTextSent`: records that the text with these keys was sent, or,
    /// with none, text without a content index.
    fn mark_text_sent(&mut self, keys: Vec<String>) {
        if keys.is_empty() {
            self.unkeyed_text_delta = true;
        } else {
            self.sent_text.extend(keys);
        }
    }

    /// `hasSentText`
    fn has_sent_text(&self, keys: &[String], has_content_index: bool) -> bool {
        (!has_content_index && self.unkeyed_text_delta)
            || keys.iter().any(|key| self.sent_text.contains(key))
    }

    /// `hasSentUnkeyedText`
    fn has_sent_unkeyed_text(&self, keys: &[String]) -> bool {
        if keys.is_empty() {
            return self.unkeyed_text_delta;
        }
        keys.iter().any(|key| self.sent_text.contains(key))
    }
}

/// `textKeysFromResponsesEvent`: the keys of the text a delta event adds
/// to.
fn text_keys_from_event(root: &Value) -> Vec<String> {
    let item_id = text(Some(root), "item_id");
    let output = root.get("output_index").map(int_of);
    match root.get("content_index").map(int_of) {
        None => unkeyed_text_keys(&item_id, output),
        content => text_keys(&item_id, output, content),
    }
}

/// `functionArgsKeysFromResponsesEvent`: the keys of the call an event is
/// about.
fn function_args_keys_from_event(root: &Value) -> Vec<String> {
    let item = root.get("item");
    let mut keys = Vec::new();
    for id in [
        text(Some(root), "item_id"),
        text(Some(root), "call_id"),
        text(item, "call_id"),
        text(item, "id"),
    ] {
        if id.is_empty() {
            continue;
        }
        let key = format!("item:{id}");
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    if let Some(output) = root.get("output_index") {
        keys.push(format!("output:{}", int_of(output)));
    }
    keys
}

/// `openAIResponsesTextKeys`: the keys of a text part with a content index.
fn text_keys(item_id: &str, output: Option<i64>, content: Option<i64>) -> Vec<String> {
    let Some(content) = content else {
        return Vec::new();
    };
    let mut keys = Vec::new();
    if !item_id.is_empty() {
        keys.push(format!("item:{item_id}:content:{content}"));
    }
    if let Some(output) = output {
        keys.push(format!("output:{output}:content:{content}"));
    }
    keys.push(format!("content:{content}"));
    keys
}

/// `openAIResponsesUnkeyedTextKeys`: the keys of a message's text as a
/// whole.
fn unkeyed_text_keys(item_id: &str, output: Option<i64>) -> Vec<String> {
    let mut keys = Vec::new();
    if !item_id.is_empty() {
        keys.push(format!("item:{item_id}"));
    }
    if let Some(output) = output {
        keys.push(format!("output:{output}"));
    }
    keys
}

/// `setInteractionsUsageFromResponses`: Interactions usage at `at`, read
/// from Responses usage. Nothing if there is none.
fn set_usage(out: &mut Value, at: &str, usage: Option<&Value>) {
    let Some(usage) = usage else {
        return;
    };
    for (from, to) in [
        ("input_tokens", &["input_tokens", "total_input_tokens"][..]),
        ("output_tokens", &["output_tokens", "total_output_tokens"]),
        ("total_tokens", &["total_tokens"]),
        (
            "input_tokens_details.cached_tokens",
            &["cached_tokens", "total_cached_tokens"],
        ),
        (
            "output_tokens_details.reasoning_tokens",
            &["reasoning_tokens", "total_thought_tokens"],
        ),
    ] {
        if let Some(value) = get(Some(usage), from) {
            let count = int_of(value);
            for key in to {
                set(out, &format!("{at}.{key}"), count);
            }
        }
    }
}

/// `ConvertOpenAIResponsesResponseToInteractionsNonStream`: a whole
/// Responses response as an Interactions one, for a request to `model`.
pub fn convert_openai_responses_response_to_interactions_non_stream(
    model: &str,
    response: &[u8],
) -> Value {
    let (root, _) = read(response);
    let root = root.as_ref();
    let mut out = json!({
        "id": "",
        "object": "interaction",
        "status": "completed",
        "model": "",
        "steps": [],
    });
    let status = text(root, "status");
    if !status.is_empty() {
        set(&mut out, "status", status);
    }
    set(&mut out, "id", text(root, "id"));
    set(&mut out, "model", response_model(model, root));
    let steps: Vec<Value> = for_each(get(root, "output"))
        .into_iter()
        .filter_map(|(_, item)| output_item_to_step(item))
        .collect();
    if !steps.is_empty() {
        set(&mut out, "steps", steps);
    }
    set_usage(&mut out, "usage", get(root, "usage"));
    out
}

/// `openAIResponsesOutputItemToInteractionsStep`: an output item as a step,
/// if it is one.
fn output_item_to_step(item: &Value) -> Option<Value> {
    match text(Some(item), "type").as_ref() {
        "message" => {
            let content: Vec<Value> = for_each(item.get("content"))
                .into_iter()
                .filter_map(|(_, part)| content_part_to_interactions(part))
                .collect();
            Some(json!({ "type": "model_output", "content": content }))
        }
        "function_call" => Some(function_call_to_interactions(item)),
        "reasoning" => {
            let content: Vec<Value> = for_each(item.get("summary"))
                .into_iter()
                .filter_map(|(_, summary)| {
                    let text = text(Some(summary), "text");
                    (!text.is_empty()).then(|| json!({ "type": "text", "text": text }))
                })
                .collect();
            Some(json!({ "type": "thought", "content": content }))
        }
        _ => None,
    }
}

/// Now, in nanoseconds since the Unix epoch.
fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// Now, in seconds since the Unix epoch.
fn unix_seconds() -> i64 {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    i64::try_from(seconds).unwrap_or(i64::MAX)
}

/// Unix seconds as Go's `time.RFC3339` writes them in UTC, such as
/// `2026-02-18T00:00:00Z`.
fn rfc3339(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let seconds = unix.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    // Go writes a negative year as `-` and at least four digits.
    let year = if year < 0 {
        format!("-{:04}", year.unsigned_abs())
    } else {
        format!("{year:04}")
    };
    format!(
        "{year}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    )
}

/// The proleptic Gregorian date `days` after 1970-01-01, after Howard
/// Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: Go's RFC 3339 layout, in UTC.
    #[test]
    fn times_are_written_as_go_writes_them() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_791_115_200), "2026-10-04T12:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(-1), "1969-12-31T23:59:59Z");
    }
}
