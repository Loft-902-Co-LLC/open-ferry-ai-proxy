// Ported from CLIProxyAPI internal/translator/openai/interactions/responses/interactions_openai_responses_response.go
// (ConvertInteractionsResponseToOpenAIResponses,
// ConvertInteractionsResponseToOpenAIResponsesNonStream,
// convertInteractionsEventToResponses, interactionsStepToResponsesOutput,
// responsesCreatedEvent, interactionsStepStartToResponses,
// interactionsStepDeltaToResponses, interactionsStepStopToResponses,
// responsesFunctionCallArgumentsDeltaToResponses,
// responsesFunctionCallArgumentsDoneToResponses,
// responsesCustomToolCallInputDoneToResponses, responsesCompletedEvent,
// responsesFailedEvent, recordResponsesReasoningSummary,
// recordResponsesTextOutput, setResponsesCompletedOutput,
// responsesFunctionCallArguments, responsesCompletedOutputItem,
// responsesReasoningItem, setResponsesUsageFromInteractions, FinalizeToolInput)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Interactions responses → OpenAI Responses events and responses.
//!
//! Each Interactions step becomes an output item at the step's index: a
//! `model_output` step a message, a `thought` step a reasoning item, and a
//! `function_call` step a function or custom tool call, named as the
//! client declared the tool. A call is announced only once its name is
//! known; until then its arguments are kept. The calls to the client's
//! `apply_patch` tool go through [`bridge`].
//!
//! Deviations from upstream: see the [module](super)'s.

mod bridge;

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt;

use serde_json::{Value, json};

use super::super::request::{
    first_existing, first_non_empty, interactions_content_part_to_responses,
    interactions_content_texts, interactions_function_call_to_responses_with_identity,
    json_string_value,
};
use super::items::{
    encrypted_content, first_usage_int, for_each, get, identity_map, is_patch, key_int,
    response_model, set, text, thought_signature,
};
use super::read::{read, sse_payload};
use crate::apply_patch::input::{CallState, InputError, failure};
use crate::common::interactions_usage::interactions_usage;
use crate::common::request_model_name;
use crate::common::responses::pick_request;
use crate::common::sse::push_event;
use crate::go;
use crate::responses_tools::{ToolIdentity, unwrap_responses_custom_tool_input};

/// The SSE frames one event gives, a chunk each.
pub(super) type Events = Vec<String>;

/// Why an `apply_patch` call, or the stream it is in, failed.
#[derive(Clone, Debug)]
pub(super) enum ToolInputError {
    /// The arguments aren't one valid input string.
    Arguments(InputError),
    /// The stream broke the protocol: upstream's message.
    Protocol(&'static str),
}

impl fmt::Display for ToolInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Arguments(error) => fmt::Display::fmt(error, f),
            Self::Protocol(message) => f.write_str(message),
        }
    }
}

impl Error for ToolInputError {}

impl From<InputError> for ToolInputError {
    fn from(error: InputError) -> Self {
        Self::Arguments(error)
    }
}

pub(super) const SOURCE_FAILED: ToolInputError =
    ToolInputError::Protocol("upstream apply_patch interaction failed");
pub(super) const UNRESOLVED: ToolInputError =
    ToolInputError::Protocol("unresolved Interactions apply_patch call identity");
const INVALID_RESPONSE: ToolInputError =
    ToolInputError::Protocol("invalid Interactions apply_patch response JSON");
const INVALID_EVENT: ToolInputError =
    ToolInputError::Protocol("invalid Interactions apply_patch event JSON");
const DELTA_AFTER_STOP: ToolInputError =
    ToolInputError::Protocol("apply_patch delta after source stop");
const LEGACY_FREEFORM: ToolInputError =
    ToolInputError::Protocol("legacy freeform arguments are invalid for apply_patch");
const DELTA_AFTER_DONE: ToolInputError =
    ToolInputError::Protocol("apply_patch delta after item completion");
const SOURCE_SNAPSHOT_CONFLICT: ToolInputError =
    ToolInputError::Protocol("apply_patch complete source conflicts with snapshot");
const UNTERMINATED: ToolInputError =
    ToolInputError::Protocol("upstream apply_patch stream ended before protocol completion");

/// `interactionsFunctionCallState`: what the events have said about one
/// call.
#[derive(Default)]
pub(super) struct Call {
    pub(super) id: String,
    pub(super) call_id: String,
    pub(super) item_id_seen: bool,
    pub(super) call_id_seen: bool,
    /// The arguments of the first snapshot, before any streamed fragments.
    pub(super) initial_arguments: String,
    /// The name upstream gave the call.
    pub(super) raw_name: String,
    /// Whether `response.output_item.added` has been sent.
    pub(super) added: bool,
    /// The `apply_patch` input decoder, once the call is announced.
    pub(super) patch: Option<CallState>,
    pub(super) pending_error: Option<ToolInputError>,
    pub(super) snapshot_arguments: String,
    pub(super) snapshot_input: String,
    pub(super) has_snapshot: bool,
    /// The name the client declared.
    pub(super) name: String,
    pub(super) namespace: String,
    pub(super) is_custom: bool,
    pub(super) arguments: String,
    /// The fragments streamed before the call could be announced, for
    /// `apply_patch` to decode once it is.
    pub(super) argument_fragments: Vec<String>,
    /// Whether upstream's `step.stop` has come.
    pub(super) source_stopped: bool,
    /// Whether the stop still has to be sent, once the call is announced.
    pub(super) stop_pending: bool,
    /// Whether the IDs were settled at the end of the response.
    pub(super) identity_finalized: bool,
    pub(super) arguments_done_emitted: bool,
    pub(super) item_done_emitted: bool,
}

/// `ConvertInteractionsResponseToOpenAIResponses`: turns an Interactions
/// event stream into Responses events. Make one per response.
pub struct InteractionsToOpenAIResponsesStream {
    pub(super) model: String,
    /// The model `response.created` names: the request's, or `model`.
    pub(super) created_model: String,
    /// What each tool the client declared is called upstream.
    pub(super) identities: HashMap<String, ToolIdentity>,
    /// Whether the client declared `apply_patch`.
    pub(super) patch_bridge: bool,
    pub(super) error: Option<ToolInputError>,
    pub(super) id: String,
    pub(super) environment_id: String,
    pub(super) calls: BTreeMap<i64, Call>,
    pub(super) item_ids: BTreeMap<i64, String>,
    pub(super) item_types: BTreeMap<i64, String>,
    pub(super) reasoning_encrypted: BTreeMap<i64, String>,
    pub(super) reasoning_summaries: BTreeMap<i64, Vec<String>>,
    pub(super) text_outputs: BTreeMap<i64, String>,
    pub(super) seq: i64,
    pub(super) done: bool,
    /// Whether the response has completed or failed.
    pub(super) terminal: bool,
    pub(super) source_failed: bool,
    /// Why an event that isn't a step's was invalid JSON.
    pub(super) pending_envelope_error: Option<ToolInputError>,
    pub(super) pending_identity_errors: BTreeMap<i64, ToolInputError>,
    pub(super) item_identity_indexes: HashMap<String, i64>,
    pub(super) call_identity_indexes: HashMap<String, i64>,
    /// Whether a chunk has been given. Upstream's state only exists from
    /// the first, so before it there is nothing to finalize.
    pub(super) given_chunk: bool,
}

impl InteractionsToOpenAIResponsesStream {
    /// A stream for a response from `model`, to the client's
    /// `original_request`, sent upstream as `request`. `Null` stands for
    /// either being absent.
    pub fn new(model: &str, original_request: &Value, request: &Value) -> Self {
        let identities = identity_map(pick_request(original_request, request));
        let patch_bridge = identities.values().any(|identity| identity.apply_patch);
        let created_model = request_model_name(original_request, request).unwrap_or(model);
        Self {
            model: model.to_owned(),
            created_model: created_model.to_owned(),
            identities,
            patch_bridge,
            error: None,
            id: String::new(),
            environment_id: String::new(),
            calls: BTreeMap::new(),
            item_ids: BTreeMap::new(),
            item_types: BTreeMap::new(),
            reasoning_encrypted: BTreeMap::new(),
            reasoning_summaries: BTreeMap::new(),
            text_outputs: BTreeMap::new(),
            seq: 0,
            done: false,
            terminal: false,
            source_failed: false,
            pending_envelope_error: None,
            pending_identity_errors: BTreeMap::new(),
            item_identity_indexes: HashMap::new(),
            call_identity_indexes: HashMap::new(),
            given_chunk: false,
        }
    }

    /// Translates one chunk of the Interactions stream, an SSE frame or a
    /// `data:` line, into the Responses events it gives, as SSE text. The
    /// end of the stream gives `data: [DONE]`, with no line break after it.
    pub fn translate(&mut self, chunk: &[u8]) -> String {
        self.given_chunk = true;
        self.event(chunk).concat()
    }

    /// `FinalizeToolInput`: call when the Interactions stream ends. If the
    /// client declared `apply_patch` and the response never completed or
    /// failed, returns `response.failed`, since a patch may be cut short.
    /// Returns `""` if no chunk was given at all, as upstream has no state
    /// to finalize then.
    pub fn finalize_tool_input(&mut self) -> String {
        if !self.given_chunk || self.error.is_some() || self.terminal || !self.patch_bridge {
            return String::new();
        }
        self.error = Some(UNTERMINATED);
        self.terminal = true;
        self.seq += 1;
        emit("response.failed", &failure(&self.id, self.seq))
    }

    /// `ToolInputError`: why the stream failed, if an `apply_patch` call, or
    /// the stream it is in, did.
    pub fn tool_input_error(&self) -> Option<&(dyn Error + 'static)> {
        self.error
            .as_ref()
            .map(|error| error as &(dyn Error + 'static))
    }

    /// `convertInteractionsEventToResponses`
    pub(super) fn event(&mut self, raw: &[u8]) -> Events {
        if self.done || self.error.is_some() || self.source_failed {
            return Vec::new();
        }
        let payload = sse_payload(raw);
        if payload.is_empty() {
            return Vec::new();
        }
        let (root, valid) = read(&payload);
        // A source sentinel remains legal after response completion, but
        // only once.
        let is_done = go::trim_space(&payload) == b"[DONE]"
            || (valid && text(root.as_ref(), "event_type") == "done");
        if is_done {
            let mut events = self.finish_patch_calls();
            if self.error.is_some() {
                return events;
            }
            self.done = true;
            self.terminal = true;
            events.push("data: [DONE]".to_owned());
            return events;
        }
        if self.terminal {
            return Vec::new();
        }
        let Some(root) = root else {
            return Vec::new();
        };
        if !valid && let Some(failed) = self.invalid_event(&root) {
            return failed;
        }
        match text(Some(&root), "event_type").as_ref() {
            "interaction.created" => vec![self.created_event(&root)],
            "step.start" => self.step_start(&root),
            "step.delta" => self.step_delta(&root),
            "step.stop" => self.step_stop(&root),
            "interaction.completed" | "finish" => self.interaction_completed(&root),
            "response.failed" | "interaction.failed" => {
                if self.patch_bridge {
                    return self.patch_failure(SOURCE_FAILED);
                }
                self.source_failed = true;
                self.terminal = true;
                vec![self.failed_event(&root)]
            }
            _ => Vec::new(),
        }
    }

    /// An event that isn't valid JSON, read as far as it goes: it fails the
    /// stream if it could carry part of a patch, and else is kept as the
    /// reason any patch found later fails.
    fn invalid_event(&mut self, root: &Value) -> Option<Events> {
        let index = match self.resolve_step_index(root.get("index"), root.get("step"), 0) {
            Ok(index) => index,
            Err(error) => return Some(self.patch_failure(error)),
        };
        if text(Some(root), "event_type").starts_with("step.") {
            let (id, call_id) = (
                text(Some(root), "step.id"),
                text(Some(root), "step.call_id"),
            );
            let call = self.calls.entry(index).or_insert_with(|| Call {
                id: id.to_string(),
                call_id: call_id.to_string(),
                item_id_seen: !id.is_empty(),
                call_id_seen: !call_id.is_empty(),
                ..Call::default()
            });
            call.pending_error.get_or_insert(INVALID_EVENT);
            if is_patch(&self.identities, &call.raw_name)
                || is_patch(&self.identities, &text(Some(root), "step.name"))
            {
                return Some(self.patch_failure(INVALID_EVENT));
            }
        } else {
            let error = self
                .pending_envelope_error
                .get_or_insert(INVALID_EVENT)
                .clone();
            if self.calls.values().any(|call| call.patch.is_some()) {
                return Some(self.patch_failure(error));
            }
        }
        None
    }

    /// `responsesCreatedEvent`
    fn created_event(&mut self, root: &Value) -> String {
        let mut payload = json!({
            "type": "response.created",
            "response": {
                "id": "",
                "object": "response",
                "status": "in_progress",
                "model": "",
                "output": [],
            },
        });
        set(&mut payload, "sequence_number", self.next_seq());
        let (own, nested) = (text(Some(root), "interaction.id"), text(Some(root), "id"));
        let id = first_non_empty([&own, &nested]);
        if !id.is_empty() {
            self.id = id.to_owned();
        }
        set(&mut payload, "response.id", id);
        set(&mut payload, "response.model", self.model.as_str());
        let environment = [
            "interaction.environment_id",
            "environment_id",
            "environment.id",
            "interaction.environment.id",
        ]
        .map(|at| text(Some(root), at));
        let environment_id = first_non_empty([
            &environment[0],
            &environment[1],
            &environment[2],
            &environment[3],
        ]);
        if !environment_id.is_empty() {
            self.environment_id = environment_id.to_owned();
            set(&mut payload, "response.environment_id", environment_id);
        }
        if !self.created_model.is_empty() {
            set(&mut payload, "response.model", self.created_model.as_str());
        }
        emit("response.created", &payload)
    }

    /// `interactionsStepStartToResponses`
    fn step_start(&mut self, root: &Value) -> Events {
        let step = root.get("step");
        let index = match self.resolve_step_index(root.get("index"), step, 0) {
            Ok(index) => index,
            Err(error) => return self.patch_failure(error),
        };
        let step_type = text(step, "type");
        // A repeated start must not overwrite a possible patch call's type
        // evidence.
        if let Some(call) = self.calls.get(&index)
            && (call.patch.is_some()
                || is_patch(&self.identities, &call.raw_name)
                || (call.raw_name.is_empty() && self.patch_bridge))
        {
            return self.update_function_call(index, step, true);
        }
        let (id, call_id) = (text(step, "id"), text(step, "call_id"));
        let item_id = first_non_empty([&id, &call_id, &format!("item_{index}")]).to_owned();
        if step_type == "function_call" {
            return self.update_function_call(index, step, true);
        }
        self.item_ids.insert(index, item_id.clone());
        self.item_types.insert(index, step_type.to_string());
        match step_type.as_ref() {
            "model_output" => {
                let mut added = json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {
                        "id": "",
                        "type": "message",
                        "status": "in_progress",
                        "role": "assistant",
                        "content": [],
                    },
                });
                set(&mut added, "sequence_number", self.next_seq());
                set(&mut added, "output_index", index);
                set(&mut added, "item.id", item_id.as_str());
                let mut part = json!({
                    "type": "response.content_part.added",
                    "output_index": 0,
                    "content_index": 0,
                    "item_id": "",
                    "part": { "type": "output_text", "text": "" },
                });
                set(&mut part, "sequence_number", self.next_seq());
                set(&mut part, "output_index", index);
                set(&mut part, "item_id", item_id);
                vec![
                    emit("response.output_item.added", &added),
                    emit("response.content_part.added", &part),
                ]
            }
            "thought" => {
                let mut added = json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {
                        "id": "",
                        "type": "reasoning",
                        "status": "in_progress",
                        "encrypted_content": "",
                        "summary": [],
                    },
                });
                set(&mut added, "sequence_number", self.next_seq());
                set(&mut added, "output_index", index);
                set(&mut added, "item.id", item_id.as_str());
                let signature = self.encrypted(index);
                if !signature.is_empty() {
                    set(&mut added, "item.encrypted_content", signature);
                }
                let mut part = json!({
                    "type": "response.reasoning_summary_part.added",
                    "item_id": "",
                    "output_index": 0,
                    "summary_index": 0,
                    "part": { "type": "summary_text", "text": "" },
                });
                set(&mut part, "sequence_number", self.next_seq());
                set(&mut part, "item_id", item_id);
                set(&mut part, "output_index", index);
                vec![
                    emit("response.output_item.added", &added),
                    emit("response.reasoning_summary_part.added", &part),
                ]
            }
            _ => Vec::new(),
        }
    }

    /// `interactionsStepDeltaToResponses`
    fn step_delta(&mut self, root: &Value) -> Events {
        let step = root.get("step");
        let index = match self.resolve_step_index(root.get("index"), step, 0) {
            Ok(index) => index,
            Err(error) => return self.patch_failure(error),
        };
        // Check the source barrier before a same-event snapshot or identity
        // update can replay fragments and publish completion. Unknown names
        // retain the violation.
        if let Some(call) = self.calls.get_mut(&index)
            && call.source_stopped
            && text(Some(root), "delta.type") == "arguments_delta"
            && !text(Some(root), "delta.arguments").is_empty()
        {
            if call.patch.is_some()
                || is_patch(&self.identities, &call.raw_name)
                || is_patch(&self.identities, &text(step, "name"))
            {
                return self.patch_failure(DELTA_AFTER_STOP);
            }
            if call.raw_name.is_empty() && self.patch_bridge {
                call.pending_error.get_or_insert(DELTA_AFTER_STOP);
            }
        }
        if let Some(step) = step.filter(|step| step.is_object()) {
            let mut events = self.update_function_call(index, Some(step), false);
            if self.terminal {
                return events;
            }
            // Process the same real delta after its late identity update,
            // without replaying the update or turning its snapshot into
            // parameter fragments.
            let mut rest = root.clone();
            if let Some(fields) = rest.as_object_mut() {
                fields.shift_remove("step");
            }
            set(&mut rest, "index", index);
            events.extend(self.step_delta(&rest));
            return events;
        }
        let delta = root.get("delta");
        match text(delta, "type").as_ref() {
            "thought_summary" => {
                let (nested, own) = (text(delta, "content.text"), text(delta, "text"));
                let summary = first_non_empty([&nested, &own]).to_owned();
                if !summary.is_empty() {
                    self.reasoning_summaries
                        .entry(index)
                        .or_default()
                        .push(summary.clone());
                }
                let mut payload = json!({
                    "type": "response.reasoning_summary_text.delta",
                    "item_id": "",
                    "output_index": 0,
                    "summary_index": 0,
                    "delta": "",
                });
                set(&mut payload, "sequence_number", self.next_seq());
                set(&mut payload, "item_id", self.item_id(index));
                set(&mut payload, "output_index", index);
                set(&mut payload, "delta", summary);
                vec![emit("response.reasoning_summary_text.delta", &payload)]
            }
            "thought_signature" => {
                let signature = encrypted_content(&text(delta, "signature"));
                if !signature.is_empty() {
                    self.reasoning_encrypted.insert(index, signature);
                }
                Vec::new()
            }
            "arguments_delta" => self.arguments_fragment(index, delta),
            _ => {
                let mut payload = json!({
                    "type": "response.output_text.delta",
                    "output_index": 0,
                    "content_index": 0,
                    "item_id": "",
                    "delta": "",
                });
                set(&mut payload, "sequence_number", self.next_seq());
                set(&mut payload, "output_index", index);
                set(&mut payload, "item_id", self.item_id(index));
                let delta = text(delta, "text");
                if !delta.is_empty() {
                    self.text_outputs.entry(index).or_default().push_str(&delta);
                }
                set(&mut payload, "delta", delta);
                vec![emit("response.output_text.delta", &payload)]
            }
        }
    }

    /// A step's `arguments_delta`.
    fn arguments_fragment(&mut self, index: i64, delta: Option<&Value>) -> Events {
        let arguments = text(delta, "arguments").into_owned();
        let call = self.calls.entry(index).or_default();
        if get(delta, "invalid_json_str").is_some() {
            call.pending_error = Some(LEGACY_FREEFORM);
            if call.patch.is_some() || is_patch(&self.identities, &call.raw_name) {
                return self.patch_failure(LEGACY_FREEFORM);
            }
        }
        if let Some(patch) = call.patch.as_mut() {
            if call.item_done_emitted && !arguments.is_empty() {
                return self.patch_failure(DELTA_AFTER_DONE);
            }
            call.arguments.push_str(&arguments);
            return match patch.push_arguments(&arguments) {
                Ok(delta) => patch_delta(patch, &mut self.seq, &delta),
                Err(error) => self.patch_failure(error.into()),
            };
        }
        if call.item_done_emitted {
            return Vec::new();
        }
        call.arguments.push_str(&arguments);
        if !call.source_stopped
            && (call.raw_name.is_empty() || is_patch(&self.identities, &call.raw_name))
        {
            call.argument_fragments.push(arguments.clone());
        }
        if call.raw_name.is_empty() || call.is_custom {
            return Vec::new();
        }
        let item_id = self.item_id(index);
        vec![self.arguments_delta(index, &item_id, &arguments)]
    }

    /// `interactionsStepStopToResponses`
    pub(super) fn step_stop(&mut self, root: &Value) -> Events {
        let step = root.get("step");
        let index = match self.resolve_step_index(root.get("index"), step, 0) {
            Ok(index) => index,
            Err(error) => return self.patch_failure(error),
        };
        // Source completion is independent of downstream identity and
        // publication, including candidates established by arguments before
        // their first start.
        if let Some(call) = self.calls.get_mut(&index) {
            call.source_stopped = true;
            if call.raw_name.is_empty() {
                call.stop_pending = true;
            }
        }
        let mut updates = Vec::new();
        if self.item_type(index) == "function_call"
            && let Some(step) = step.filter(|step| step.is_object())
        {
            updates = self.update_function_call(index, Some(step), false);
            if self.terminal {
                return updates;
            }
        }
        let item_id = self.item_id(index);
        match self.item_type(index) {
            "model_output" => self.message_done(index, &item_id),
            "function_call" => self.call_done(index, &item_id, updates),
            "thought" => self.reasoning_done(index, &item_id),
            _ => {
                let mut done = json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {},
                });
                set(&mut done, "sequence_number", self.next_seq());
                set(&mut done, "output_index", index);
                set(&mut done, "item", self.reasoning_item(index));
                vec![emit("response.output_item.done", &done)]
            }
        }
    }

    /// A reasoning item's stop: its summary text, its summary part and
    /// itself done. The summary is one part, with all the summary text.
    fn reasoning_done(&mut self, index: i64, item_id: &str) -> Events {
        let text = self.reasoning_summary(index);
        let mut text_done = json!({
            "type": "response.reasoning_summary_text.done",
            "item_id": "",
            "output_index": 0,
            "summary_index": 0,
            "text": "",
        });
        set(&mut text_done, "sequence_number", self.next_seq());
        set(&mut text_done, "item_id", item_id);
        set(&mut text_done, "output_index", index);
        set(&mut text_done, "text", text.as_str());
        let mut part = json!({
            "type": "response.reasoning_summary_part.done",
            "item_id": "",
            "output_index": 0,
            "summary_index": 0,
            "part": { "type": "summary_text", "text": "" },
        });
        set(&mut part, "sequence_number", self.next_seq());
        set(&mut part, "item_id", item_id);
        set(&mut part, "output_index", index);
        set(&mut part, "part.text", text);
        let mut done = json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {},
        });
        set(&mut done, "sequence_number", self.next_seq());
        set(&mut done, "output_index", index);
        set(&mut done, "item", self.reasoning_item(index));
        vec![
            emit("response.reasoning_summary_text.done", &text_done),
            emit("response.reasoning_summary_part.done", &part),
            emit("response.output_item.done", &done),
        ]
    }

    /// A message's stop: its text, its part and itself done.
    fn message_done(&mut self, index: i64, item_id: &str) -> Events {
        let text = self.text_outputs.get(&index).cloned().unwrap_or_default();
        let mut text_done = json!({
            "type": "response.output_text.done",
            "output_index": 0,
            "content_index": 0,
            "item_id": "",
            "text": "",
            "logprobs": [],
        });
        set(&mut text_done, "sequence_number", self.next_seq());
        set(&mut text_done, "output_index", index);
        set(&mut text_done, "item_id", item_id);
        set(&mut text_done, "text", text.as_str());
        let mut part = json!({
            "type": "response.content_part.done",
            "output_index": 0,
            "content_index": 0,
            "item_id": "",
            "part": { "type": "output_text", "text": "" },
        });
        set(&mut part, "sequence_number", self.next_seq());
        set(&mut part, "output_index", index);
        set(&mut part, "item_id", item_id);
        set(&mut part, "part.text", text.as_str());
        let mut done = json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "id": "",
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [],
            },
        });
        set(&mut done, "sequence_number", self.next_seq());
        set(&mut done, "output_index", index);
        set(&mut done, "item.id", item_id);
        if let Some(Value::Array(content)) = done.pointer_mut("/item/content") {
            content.push(json!({ "type": "output_text", "text": text }));
        }
        vec![
            emit("response.output_text.done", &text_done),
            emit("response.content_part.done", &part),
            emit("response.output_item.done", &done),
        ]
    }

    /// A call's stop: its arguments or input done and itself done, after
    /// `updates`, unless it can't be announced yet.
    fn call_done(&mut self, index: i64, item_id: &str, updates: Events) -> Events {
        let call = self.calls.entry(index).or_insert_with(|| Call {
            id: item_id.to_owned(),
            source_stopped: true,
            ..Call::default()
        });
        if call.raw_name.is_empty()
            || (is_patch(&self.identities, &call.raw_name) && call.patch.is_none())
        {
            call.stop_pending = true;
            return updates;
        }
        if call.item_done_emitted {
            return updates;
        }
        let mut events = updates;
        if let Some(patch) = call.patch.as_mut() {
            let arguments = if call.has_snapshot {
                call.snapshot_arguments.clone()
            } else {
                call.arguments.clone()
            };
            if call.has_snapshot && go::gjson_valid(call.arguments.as_bytes()) {
                match CallState::default().finish_arguments(&call.arguments) {
                    Err(error) => return self.patch_failure(error.into()),
                    Ok((_, input)) if input != call.snapshot_input => {
                        return self.patch_failure(SOURCE_SNAPSHOT_CONFLICT);
                    }
                    Ok(_) => {}
                }
            }
            let (tail, input) = match patch.finish_arguments(&arguments) {
                Ok(finished) => finished,
                Err(error) => return self.patch_failure(error.into()),
            };
            events.extend(patch_delta(patch, &mut self.seq, &tail));
            self.seq += 1;
            let input_done = patch.input_done(&input, self.seq);
            events.push(emit("response.custom_tool_call_input.done", &input_done));
            call.item_done_emitted = true;
            call.arguments_done_emitted = true;
            let item = self.completed_output_item(index, "function_call");
            let mut done = json!({ "type": "response.output_item.done" });
            set(&mut done, "sequence_number", self.next_seq());
            set(&mut done, "output_index", index);
            set(&mut done, "item", item.unwrap_or_default());
            events.push(emit("response.output_item.done", &done));
            return events;
        }
        let send_done = !call.arguments_done_emitted;
        call.arguments_done_emitted = true;
        call.item_done_emitted = true;
        let (call_id, name, namespace) = (
            call.call_id.clone(),
            call.name.clone(),
            call.namespace.clone(),
        );
        let mut done = if call.is_custom {
            let input = unwrap_responses_custom_tool_input(&call.arguments);
            if send_done {
                events.push(self.custom_input_done(index, item_id, &input));
            }
            let mut done = json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": "",
                    "type": "custom_tool_call",
                    "call_id": "",
                    "name": "",
                    "input": "",
                    "status": "completed",
                },
            });
            set(&mut done, "item.input", input);
            done
        } else {
            let arguments = function_call_arguments(Some(call)).to_owned();
            if send_done {
                events.push(self.arguments_done(index, item_id, &arguments));
            }
            let mut done = json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": "",
                    "type": "function_call",
                    "call_id": "",
                    "name": "",
                    "arguments": "",
                    "status": "completed",
                },
            });
            set(&mut done, "item.arguments", arguments);
            done
        };
        // The input or arguments, set above, keep their place in the
        // template; the fields below are set in upstream's order.
        set(&mut done, "sequence_number", self.next_seq());
        set(&mut done, "output_index", index);
        set(&mut done, "item.id", item_id);
        set(&mut done, "item.call_id", call_id);
        if !namespace.is_empty() {
            set(&mut done, "item.namespace", namespace);
        }
        set(&mut done, "item.name", name);
        events.push(emit("response.output_item.done", &done));
        events
    }

    /// The interaction's end: the calls left to finish, then
    /// `response.completed`.
    fn interaction_completed(&mut self, root: &Value) -> Events {
        let mut events = Vec::new();
        let steps = first_existing([get(Some(root), "interaction.steps"), root.get("steps")]);
        for (key, step) in for_each(steps) {
            let index =
                match self.resolve_step_index(step.get("index"), Some(step), key_int(key.as_ref()))
                {
                    Ok(index) => index,
                    Err(error) => {
                        events.extend(self.patch_failure(error));
                        break;
                    }
                };
            let call = self.calls.get(&index);
            // Patch-enabled unnamed functions retain evidence before final
            // snapshot filtering.
            let ordinary = call.is_none_or(|call| {
                call.patch.is_none()
                    && !is_patch(&self.identities, &call.raw_name)
                    && (!self.patch_bridge || !call.raw_name.is_empty())
            });
            if ordinary {
                if text(Some(step), "type") != "function_call" {
                    continue;
                }
                let name = text(Some(step), "name");
                let unresolved = self.patch_bridge
                    && (name.is_empty() || call.is_some_and(|call| call.raw_name.is_empty()));
                if !is_patch(&self.identities, &name) && !unresolved {
                    continue;
                }
            }
            events.extend(self.update_function_call(index, Some(step), false));
            if self.terminal {
                break;
            }
        }
        if self.terminal {
            return events;
        }
        events.extend(self.finish_patch_calls());
        if self.terminal {
            return events;
        }
        self.terminal = true;
        events.push(self.completed_event(root));
        events
    }

    /// `responsesCompletedEvent`
    fn completed_event(&mut self, root: &Value) -> String {
        let interaction = root.get("interaction");
        let both = |at| (text(interaction, at), text(Some(root), at));
        let (status, root_status) = both("status");
        let (reason, root_reason) = both("finish_reason");
        let status = first_non_empty([&status, &root_status]);
        let reason = first_non_empty([&reason, &root_reason]);
        let (event_type, status, incomplete) = if reason == "content_filter" {
            ("response.incomplete", "incomplete", "content_filter")
        } else if status == "incomplete" || reason == "length" || reason == "max_tokens" {
            ("response.incomplete", "incomplete", "max_output_tokens")
        } else {
            ("response.completed", "completed", "")
        };
        let mut payload = json!({
            "type": "response.completed",
            "response": {
                "id": "",
                "object": "response",
                "status": "completed",
                "model": "",
                "output": [],
                "usage": {},
            },
        });
        set(&mut payload, "type", event_type);
        set(&mut payload, "response.status", status);
        if !incomplete.is_empty() {
            set(
                &mut payload,
                "response.incomplete_details.reason",
                incomplete,
            );
        }
        set(&mut payload, "sequence_number", self.next_seq());
        let (id, root_id) = both("id");
        set(
            &mut payload,
            "response.id",
            first_non_empty([&id, &root_id]),
        );
        let model = text(interaction, "model");
        set(
            &mut payload,
            "response.model",
            first_non_empty([&model, &self.model]),
        );
        let environment = [
            text(interaction, "environment_id"),
            text(Some(root), "environment_id"),
            text(interaction, "environment.id"),
            text(Some(root), "environment.id"),
        ];
        let mut environment_id = first_non_empty([
            &environment[0],
            &environment[1],
            &environment[2],
            &environment[3],
        ]);
        if environment_id.is_empty() {
            environment_id = &self.environment_id;
        }
        if !environment_id.is_empty() {
            set(&mut payload, "response.environment_id", environment_id);
        }
        let output: Vec<Value> = self
            .item_types
            .range(0..)
            .filter_map(|(&index, item_type)| self.completed_output_item(index, item_type))
            .collect();
        if !output.is_empty() {
            set(&mut payload, "response.output", output);
        }
        set_usage(&mut payload, "response.usage", interactions_usage(root));
        emit(event_type, &payload)
    }

    /// `responsesFailedEvent`
    fn failed_event(&mut self, root: &Value) -> String {
        let mut payload = json!({
            "type": "response.failed",
            "response": {
                "id": "",
                "object": "response",
                "status": "failed",
                "model": "",
                "output": [],
                "error": { "message": "", "code": "", "type": "server_error" },
            },
        });
        set(&mut payload, "sequence_number", self.next_seq());
        let interaction = root.get("interaction");
        let (id, root_id) = (text(interaction, "id"), text(Some(root), "id"));
        let mut id = first_non_empty([&id, &root_id]);
        if id.is_empty() {
            id = &self.id;
        }
        set(&mut payload, "response.id", id);
        let model = text(interaction, "model");
        set(
            &mut payload,
            "response.model",
            first_non_empty([&model, &self.model]),
        );
        let error = match root.get("error") {
            None => interaction.and_then(|interaction| interaction.get("error")),
            found => found,
        };
        let mut message = text(error, "message");
        if message.is_empty() {
            message = "upstream execution failed".into();
        }
        set(&mut payload, "response.error.message", message);
        let code = text(error, "code");
        if code.is_empty() {
            if let Some(Value::Object(fields)) = payload.pointer_mut("/response/error") {
                fields.shift_remove("code");
            }
        } else {
            set(&mut payload, "response.error.code", code);
        }
        let mut error_type = text(error, "type");
        if error_type.is_empty() {
            error_type = "server_error".into();
        }
        set(&mut payload, "response.error.type", error_type);
        emit("response.failed", &payload)
    }

    /// `responsesCompletedOutputItem`: the item at `index` as the completed
    /// response lists it, if it is one.
    pub(super) fn completed_output_item(&self, index: i64, item_type: &str) -> Option<Value> {
        let item_id = self.item_id(index);
        match item_type {
            "model_output" => {
                let mut item = json!({
                    "id": "",
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [],
                });
                set(&mut item, "id", item_id);
                if let Some(text) = self
                    .text_outputs
                    .get(&index)
                    .filter(|text| !text.is_empty())
                {
                    set(
                        &mut item,
                        "content",
                        json!([{ "type": "output_text", "text": text }]),
                    );
                }
                Some(item)
            }
            "thought" => Some(self.reasoning_item(index)),
            "function_call" => {
                let call = self.calls.get(&index);
                if let Some(call) = call.filter(|call| call.is_custom) {
                    let mut item = json!({
                        "id": "",
                        "type": "custom_tool_call",
                        "call_id": "",
                        "name": "",
                        "input": "",
                        "status": "completed",
                    });
                    set(&mut item, "id", item_id);
                    set(&mut item, "call_id", call.call_id.as_str());
                    if !call.namespace.is_empty() {
                        set(&mut item, "namespace", call.namespace.as_str());
                    }
                    set(&mut item, "name", call.name.as_str());
                    let input = match &call.patch {
                        Some(patch) => patch.input().to_owned(),
                        None => {
                            unwrap_responses_custom_tool_input(function_call_arguments(Some(call)))
                        }
                    };
                    set(&mut item, "input", input);
                    return Some(item);
                }
                let mut item = json!({
                    "id": "",
                    "type": "function_call",
                    "call_id": "",
                    "name": "",
                    "arguments": "{}",
                    "status": "completed",
                });
                set(&mut item, "id", item_id.as_str());
                set(&mut item, "call_id", item_id);
                if let Some(call) = call {
                    set(&mut item, "call_id", call.call_id.as_str());
                    if !call.namespace.is_empty() {
                        set(&mut item, "namespace", call.namespace.as_str());
                    }
                    set(&mut item, "name", call.name.as_str());
                    set(&mut item, "arguments", function_call_arguments(Some(call)));
                }
                Some(item)
            }
            _ => None,
        }
    }

    /// `responsesReasoningItem`: completed, with one summary part holding
    /// all the summary text, empty if there was none.
    fn reasoning_item(&self, index: i64) -> Value {
        let mut item = json!({
            "id": "",
            "type": "reasoning",
            "status": "completed",
            "encrypted_content": "",
            "summary": [],
        });
        set(&mut item, "id", self.item_id(index));
        let signature = self.encrypted(index);
        if !signature.is_empty() {
            set(&mut item, "encrypted_content", signature);
        }
        let part = json!({ "type": "summary_text", "text": self.reasoning_summary(index) });
        set(&mut item, "summary", vec![part]);
        item
    }

    /// The summary text of the reasoning item at `index`, its fragments
    /// joined.
    fn reasoning_summary(&self, index: i64) -> String {
        self.reasoning_summaries
            .get(&index)
            .map(|summaries| summaries.concat())
            .unwrap_or_default()
    }

    /// `responsesFunctionCallArgumentsDeltaToResponses`
    pub(super) fn arguments_delta(&mut self, index: i64, item_id: &str, arguments: &str) -> String {
        let mut payload = json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "item_id": "",
            "delta": "",
        });
        set(&mut payload, "sequence_number", self.next_seq());
        set(&mut payload, "output_index", index);
        set(&mut payload, "item_id", item_id);
        set(&mut payload, "delta", arguments);
        emit("response.function_call_arguments.delta", &payload)
    }

    /// `responsesFunctionCallArgumentsDoneToResponses`
    fn arguments_done(&mut self, index: i64, item_id: &str, arguments: &str) -> String {
        let mut payload = json!({
            "type": "response.function_call_arguments.done",
            "output_index": 0,
            "item_id": "",
            "arguments": "",
        });
        set(&mut payload, "sequence_number", self.next_seq());
        set(&mut payload, "output_index", index);
        set(&mut payload, "item_id", item_id);
        set(&mut payload, "arguments", arguments);
        emit("response.function_call_arguments.done", &payload)
    }

    /// `responsesCustomToolCallInputDoneToResponses`
    fn custom_input_done(&mut self, index: i64, item_id: &str, input: &str) -> String {
        let mut payload = json!({
            "type": "response.custom_tool_call_input.done",
            "output_index": 0,
            "item_id": "",
            "input": "",
        });
        set(&mut payload, "sequence_number", self.next_seq());
        set(&mut payload, "output_index", index);
        set(&mut payload, "item_id", item_id);
        set(&mut payload, "input", input);
        emit("response.custom_tool_call_input.done", &payload)
    }

    /// The recognized signature kept for the reasoning item at `index`.
    fn encrypted(&self, index: i64) -> String {
        encrypted_content(
            self.reasoning_encrypted
                .get(&index)
                .map_or("", String::as_str),
        )
    }

    /// `ItemIDs[index]`, `""` if there is none.
    fn item_id(&self, index: i64) -> String {
        self.item_ids.get(&index).cloned().unwrap_or_default()
    }

    /// `ItemTypes[index]`, `""` if there is none.
    fn item_type(&self, index: i64) -> &str {
        self.item_types.get(&index).map_or("", String::as_str)
    }

    /// `nextResponsesSeq`
    fn next_seq(&mut self) -> i64 {
        self.seq += 1;
        self.seq
    }

    /// `interactionsPatchFailure`: the one `response.failed` event, which
    /// ends the stream with `error`. Nothing once the response has ended.
    pub(super) fn patch_failure(&mut self, error: ToolInputError) -> Events {
        if self.terminal {
            return Vec::new();
        }
        self.error = Some(error);
        self.terminal = true;
        let seq = self.next_seq();
        vec![emit("response.failed", &failure(&self.id, seq))]
    }
}

/// `interactionsPatchDelta`: a `response.custom_tool_call_input.delta`
/// event for a non-empty `delta`.
pub(super) fn patch_delta(patch: &CallState, seq: &mut i64, delta: &str) -> Events {
    if delta.is_empty() {
        return Vec::new();
    }
    *seq += 1;
    vec![emit(
        "response.custom_tool_call_input.delta",
        &patch.input_delta(delta, *seq),
    )]
}

/// `responsesFunctionCallArguments`: the arguments so far, or `{}`.
fn function_call_arguments(call: Option<&Call>) -> &str {
    match call {
        Some(call) if !call.arguments.is_empty() => &call.arguments,
        _ => "{}",
    }
}

/// `SSEEventData`: one SSE frame.
pub(super) fn emit(event: &str, payload: &Value) -> String {
    let mut out = String::new();
    push_event(&mut out, event, payload);
    out
}

/// `setResponsesUsageFromInteractions`: Responses usage at `at`, read from
/// Interactions usage. The three totals are always written.
fn set_usage(out: &mut Value, at: &str, usage: Option<&Value>) {
    let (mut input, mut output, mut total) = (0_i64, 0_i64, 0_i64);
    if let Some(usage) = usage {
        input = first_usage_int(usage, &["input_tokens", "total_input_tokens"]).unwrap_or(0);
        output = first_usage_int(usage, &["output_tokens", "total_output_tokens"]).unwrap_or(0);
        total = first_usage_int(usage, &["total_tokens"]).unwrap_or(input.wrapping_add(output));
    }
    set(out, &format!("{at}.input_tokens"), input);
    set(out, &format!("{at}.output_tokens"), output);
    set(out, &format!("{at}.total_tokens"), total);
    let Some(usage) = usage else {
        return;
    };
    if let Some(cached) = first_usage_int(usage, &["cached_tokens", "total_cached_tokens"]) {
        set(
            out,
            &format!("{at}.input_tokens_details.cached_tokens"),
            cached,
        );
    }
    if let Some(reasoning) = first_usage_int(usage, &["reasoning_tokens", "total_thought_tokens"]) {
        set(
            out,
            &format!("{at}.output_tokens_details.reasoning_tokens"),
            reasoning,
        );
    }
}

/// `ConvertInteractionsResponseToOpenAIResponsesNonStream`: a whole
/// Interactions response as a Responses one, for a request to `model`.
/// `None` if an `apply_patch` call in it is unusable, or the interaction
/// failed while the client declared `apply_patch`.
pub fn convert_interactions_response_to_openai_responses_non_stream(
    model: &str,
    original_request: &Value,
    request: &Value,
    response: &[u8],
) -> Option<Value> {
    non_stream(model, original_request, request, response).ok()
}

/// [`convert_interactions_response_to_openai_responses_non_stream`], with
/// why it failed.
pub(super) fn non_stream(
    model: &str,
    original_request: &Value,
    request: &Value,
    response: &[u8],
) -> Result<Value, ToolInputError> {
    let (root, _) = read(response);
    let root = root.as_ref();
    let mut out = json!({
        "id": "",
        "object": "response",
        "status": "completed",
        "model": "",
        "output": [],
    });
    let (id, nested_id) = (text(root, "id"), text(root, "interaction.id"));
    set(&mut out, "id", first_non_empty([&id, &nested_id]));
    set(&mut out, "model", response_model(model, root));
    let steps = get(root, "steps").or_else(|| get(root, "interaction.steps"));
    let identities = identity_map(pick_request(original_request, request));
    let patch_enabled = identities.values().any(|identity| identity.apply_patch);
    let (status, nested_status) = (text(root, "status"), text(root, "interaction.status"));
    let status = first_non_empty([&status, &nested_status]);
    let source_error = first_existing([get(root, "error"), get(root, "interaction.error")]);
    if patch_enabled && (status == "failed" || source_error.is_some_and(|error| !error.is_null())) {
        return Err(SOURCE_FAILED);
    }
    let mut output = Vec::new();
    for (_, step) in for_each(steps) {
        let (step_type, name) = (text(Some(step), "type"), text(Some(step), "name"));
        if patch_enabled && step_type == "function_call" && name.is_empty() {
            return Err(UNRESOLVED);
        }
        if step_type == "function_call" && is_patch(&identities, &name) {
            if !go::gjson_valid(response) {
                return Err(INVALID_RESPONSE);
            }
            let arguments = json_string_value(step.get("arguments"), "{}");
            CallState::default().finish_arguments(arguments)?;
        }
        if let Some(item) = step_to_output(step, &identities) {
            output.push(item);
        }
    }
    if !output.is_empty() {
        set(&mut out, "output", output);
    }
    let (reason, nested_reason) = (
        text(root, "finish_reason"),
        text(root, "interaction.finish_reason"),
    );
    let reason = first_non_empty([&reason, &nested_reason]);
    if reason == "content_filter" {
        set(&mut out, "status", "incomplete");
        set(&mut out, "incomplete_details.reason", "content_filter");
    } else if status == "incomplete" || reason == "length" || reason == "max_tokens" {
        set(&mut out, "status", "incomplete");
        set(&mut out, "incomplete_details.reason", "max_output_tokens");
    }
    let environment = [
        "environment_id",
        "interaction.environment_id",
        "environment.id",
        "interaction.environment.id",
    ]
    .map(|at| text(root, at));
    let environment_id = first_non_empty([
        &environment[0],
        &environment[1],
        &environment[2],
        &environment[3],
    ]);
    if !environment_id.is_empty() {
        set(&mut out, "environment_id", environment_id);
    }
    set_usage(&mut out, "usage", root.and_then(interactions_usage));
    Ok(out)
}

/// `interactionsStepToResponsesOutput`: a step as a Responses output item,
/// if it is one.
fn step_to_output(step: &Value, identities: &HashMap<String, ToolIdentity>) -> Option<Value> {
    match text(Some(step), "type").as_ref() {
        "model_output" => {
            let mut item = json!({ "type": "message", "role": "assistant", "content": [] });
            let (id, step_id) = (text(Some(step), "id"), text(Some(step), "step_id"));
            let id = first_non_empty([&id, &step_id]);
            if !id.is_empty() {
                set(&mut item, "id", id);
            }
            let content = step.get("content");
            let parts: Vec<Value> = match content {
                Some(Value::String(text)) => vec![json!({ "type": "output_text", "text": text })],
                _ => for_each(content)
                    .into_iter()
                    .filter_map(|(_, part)| {
                        interactions_content_part_to_responses(part, "assistant")
                    })
                    .collect(),
            };
            if !parts.is_empty() {
                set(&mut item, "content", parts);
            }
            Some(item)
        }
        "thought" => {
            let mut item = json!({ "type": "reasoning", "summary": [] });
            let signature = thought_signature(step);
            if !signature.is_empty() {
                set(&mut item, "encrypted_content", signature);
            }
            let summaries: Vec<Value> = interactions_content_texts(step.get("content"))
                .into_iter()
                .map(|text| json!({ "type": "summary_text", "text": text }))
                .collect();
            if !summaries.is_empty() {
                set(&mut item, "summary", summaries);
            }
            Some(item)
        }
        "function_call" => {
            let mut item =
                interactions_function_call_to_responses_with_identity(step, Some(identities));
            set(&mut item, "status", "completed");
            Some(item)
        }
        _ => None,
    }
}
