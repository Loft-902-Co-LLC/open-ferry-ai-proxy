// Ported from CLIProxyAPI internal/translator/openai/interactions/responses/interactions_openai_responses_response.go
// (interactionsResolveStepIndex, interactionsHasPatchBridge,
// interactionsUpdateFunctionCall, interactionsFinishPatchCalls) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Settling which call each step event is about, and announcing a call once
//! its name is known.
//!
//! A call to the client's `apply_patch` tool is announced as a custom tool
//! call only once upstream has given both its IDs, or the response has
//! ended, and its streamed arguments are decoded into the patch text as
//! they come. Steps whose index and IDs disagree, snapshots of one call
//! that disagree, arguments after the call stopped and a call never named
//! all fail the stream with `response.failed` if a patch is involved; an
//! ordinary function call keeps its looser handling.
//!
//! Deviations from upstream: see the [module](super::super)'s.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use super::super::items::{first_non_empty, get, is_patch, json_string_value, set, text};
use super::{
    Call, Events, InteractionsToOpenAIResponsesStream, ToolInputError, UNRESOLVED, emit,
    patch_delta,
};
use crate::apply_patch::input::CallState;
use crate::common::responses::set_tool_call_identity;
use crate::json::{int_of, str_of};

const CONFLICTING_STEP_IDENTITY: ToolInputError =
    ToolInputError::Protocol("conflicting Interactions apply_patch step identity");
const CONFLICTING_TYPE: ToolInputError =
    ToolInputError::Protocol("conflicting apply_patch item type");
const CONFLICTING_IDENTITY: ToolInputError =
    ToolInputError::Protocol("conflicting apply_patch call identity");
const CONFLICTING_NAME: ToolInputError =
    ToolInputError::Protocol("conflicting apply_patch call name");
const CONFLICTING_SNAPSHOTS: ToolInputError =
    ToolInputError::Protocol("conflicting apply_patch full snapshots");
const SNAPSHOT_AFTER_DONE: ToolInputError =
    ToolInputError::Protocol("apply_patch snapshot conflicts with completed input");

impl InteractionsToOpenAIResponsesStream {
    /// `interactionsResolveStepIndex`: the index of the item a step event
    /// is about, from its `index` (`explicit`), its step's `index`, or else
    /// the IDs it gives, else `fallback`. Every index and ID given is
    /// reconciled before the step's type is looked at; if they disagree, the
    /// items involved fail if any is a patch.
    pub(in super::super) fn resolve_step_index(
        &mut self,
        explicit: Option<&Value>,
        step: Option<&Value>,
        fallback: i64,
    ) -> Result<i64, ToolInputError> {
        let step_index = get(step, "index");
        let indexed = explicit.is_some() || step_index.is_some();
        let explicit = explicit.map(int_of);
        let step_index = step_index.map(int_of);
        let mut index = explicit.or(step_index).unwrap_or(fallback);
        let (item_id, call_id) = (text(step, "id"), text(step, "call_id"));
        let mut matched = BTreeSet::new();
        if !item_id.is_empty()
            && let Some(&call_index) = self.item_identity_indexes.get(item_id.as_ref())
        {
            matched.insert(call_index);
        }
        if !call_id.is_empty()
            && let Some(&call_index) = self.call_identity_indexes.get(call_id.as_ref())
        {
            matched.insert(call_index);
        }
        if !indexed && let Some(&first) = matched.first() {
            // The fallback array position is not evidence when a supplied
            // alias matches.
            index = first;
        }
        for (&call_index, call) in &self.calls {
            if (!item_id.is_empty() && (call.item_id_seen || call.added) && item_id == call.id)
                || (!call_id.is_empty()
                    && (call.call_id_seen || call.added)
                    && call_id == call.call_id)
            {
                if !indexed && (matched.is_empty() || call_index < index) {
                    index = call_index;
                }
                matched.insert(call_index);
            }
        }
        if !indexed && matched.is_empty() {
            let found = !item_id.is_empty()
                && match self.item_ids.iter().find(|(_, id)| **id == item_id) {
                    Some((&item_index, _)) => {
                        index = item_index;
                        true
                    }
                    None => false,
                };
            // A final array position must not rebind an unrelated item with
            // a different supplied ID.
            if !found && (!item_id.is_empty() || !call_id.is_empty()) {
                while self.calls.contains_key(&index) || !self.item_type(index).is_empty() {
                    index = index.wrapping_add(1);
                }
            }
        }
        let mut related = BTreeSet::from([index]);
        related.extend(explicit);
        related.extend(step_index);
        let mut conflict =
            matched.len() > 1 || matches!((explicit, step_index), (Some(a), Some(b)) if a != b);
        for &call_index in &matched {
            related.insert(call_index);
            if explicit.is_some_and(|i| i != call_index)
                || step_index.is_some_and(|i| i != call_index)
            {
                conflict = true;
            }
        }
        let mut patch_related = is_patch(&self.identities, &text(step, "name"));
        for call_index in &related {
            if let Some(call) = self.calls.get(call_index) {
                patch_related = patch_related
                    || call.patch.is_some()
                    || is_patch(&self.identities, &call.raw_name);
                if (!item_id.is_empty() && call.item_id_seen && item_id != call.id)
                    || (!call_id.is_empty() && call.call_id_seen && call_id != call.call_id)
                {
                    conflict = true;
                }
            }
        }
        if conflict {
            // Retain both sides even when an unnamed non-function snapshot
            // is skipped.
            for &call_index in &related {
                self.pending_identity_errors
                    .entry(call_index)
                    .or_insert(CONFLICTING_STEP_IDENTITY);
                if let Some(call) = self.calls.get_mut(&call_index) {
                    call.pending_error.get_or_insert(CONFLICTING_STEP_IDENTITY);
                }
            }
        }
        // Keep unmatched aliases even on conflicts or partial invalid
        // snapshots, so later provenance through any supplied key cannot
        // erase the contradiction.
        if !item_id.is_empty() {
            self.item_identity_indexes
                .entry(item_id.into_owned())
                .or_insert(index);
        }
        if !call_id.is_empty() {
            self.call_identity_indexes
                .entry(call_id.into_owned())
                .or_insert(index);
        }
        if patch_related
            && let Some(error) = related
                .iter()
                .find_map(|call_index| self.pending_identity_errors.get(call_index))
        {
            return Err(error.clone());
        }
        // Ordinary functions retain their explicit-index behavior; only
        // patch identities fail closed.
        Ok(index)
    }

    /// `interactionsUpdateFunctionCall`: records what `step` says about the
    /// call at `index`, and announces the call once it can be. `initial` is
    /// for a `step.start`, whose `{}` arguments are a placeholder. Evidence
    /// is kept even before the name identifies the winning declaration.
    pub(in super::super) fn update_function_call(
        &mut self,
        index: i64,
        step: Option<&Value>,
        initial: bool,
    ) -> Events {
        let call = self.calls.entry(index).or_default();
        if let Some(kind) = get(step, "type")
            && str_of(Some(kind)) != "function_call"
        {
            record(call, CONFLICTING_TYPE);
        }
        let patch_name = is_patch(&self.identities, &call.raw_name);
        merge_id(call, false, &text(step, "id"), patch_name);
        merge_id(call, true, &text(step, "call_id"), patch_name);
        let step_name = text(step, "name");
        if !step_name.is_empty() {
            if !call.raw_name.is_empty() && call.raw_name != step_name {
                record(call, CONFLICTING_NAME);
            } else {
                call.raw_name = step_name.to_string();
            }
        }
        if let Some(error) = &call.pending_error
            && is_patch(&self.identities, &step_name)
        {
            let error = error.clone();
            return self.patch_failure(error);
        }
        if let Some(arguments) = get(step, "arguments") {
            let placeholder = initial
                && !call.has_snapshot
                && call.arguments.is_empty()
                && !call.item_done_emitted
                && json_string_value(Some(arguments), "").trim() == "{}";
            if !placeholder {
                snapshot(call, json_string_value(Some(arguments), "{}"));
            }
        }
        self.item_ids.insert(index, call.id.clone());
        self.item_types.insert(index, "function_call".to_owned());
        if call.raw_name.is_empty() {
            return Vec::new();
        }
        let identity = self.identities.get(&call.raw_name);
        let apply_patch = identity.is_some_and(|identity| identity.apply_patch);
        match identity {
            Some(identity) => {
                call.name.clone_from(&identity.name);
                call.namespace.clone_from(&identity.namespace);
                call.is_custom = identity.custom;
            }
            None => call.name.clone_from(&call.raw_name),
        }
        if apply_patch {
            if let Some(error) = &self.pending_envelope_error {
                let error = error.clone();
                return self.patch_failure(error);
            }
            if let Some(error) = &call.pending_error {
                let error = error.clone();
                return self.patch_failure(error);
            }
            // Upstream evidence and downstream readiness are independent. A
            // first late ID may still be adopted; no provisional patch
            // identity has escaped.
            if !(call.item_id_seen && call.call_id_seen) && !call.identity_finalized {
                return Vec::new();
            }
        } else {
            call.argument_fragments.clear();
            if !call.item_id_seen && !call.added {
                call.id = first_non_empty([&call.call_id, &format!("item_{index}")]).to_owned();
            }
            if !call.call_id_seen && !call.added {
                call.call_id.clone_from(&call.id);
            }
        }
        self.item_ids.insert(index, call.id.clone());
        let mut events = Vec::new();
        // Replay buffered ordinary arguments only when the item is first
        // announced.
        let announced = !call.added;
        if announced {
            if !apply_patch && !call.initial_arguments.is_empty() {
                call.arguments.insert_str(0, &call.initial_arguments);
            }
            let (item_type, input_key) = if call.is_custom {
                ("custom_tool_call", "item.input")
            } else {
                ("function_call", "item.arguments")
            };
            self.seq += 1;
            let mut added = json!({
                "type": "response.output_item.added",
                "item": { "status": "in_progress" },
            });
            set(&mut added, "sequence_number", self.seq);
            set(&mut added, "output_index", index);
            set(&mut added, "item.type", item_type);
            set(&mut added, input_key, "");
            set(&mut added, "item.id", call.id.as_str());
            set(&mut added, "item.call_id", call.call_id.as_str());
            if let Some(item) = added.get_mut("item") {
                set_tool_call_identity(item, &call.name, &call.namespace);
            }
            events.push(emit("response.output_item.added", &added));
            call.added = true;
        }
        let mut replay = None;
        if apply_patch && call.patch.is_none() {
            let patch =
                call.patch
                    .insert(CallState::new(call.id.clone(), call.call_id.clone(), index));
            let mut failed = None;
            for fragment in &call.argument_fragments {
                match patch.push_arguments(fragment) {
                    Ok(delta) => events.extend(patch_delta(patch, &mut self.seq, &delta)),
                    Err(error) => {
                        failed = Some(error);
                        break;
                    }
                }
            }
            if let Some(error) = failed {
                events.extend(self.patch_failure(error.into()));
                return events;
            }
            call.argument_fragments.clear();
        } else if !call.is_custom && announced && !call.arguments.is_empty() {
            replay = Some((call.id.clone(), call.arguments.clone()));
        }
        let stop = call.stop_pending && call.patch.is_some();
        if stop {
            call.stop_pending = false;
        }
        if let Some((id, arguments)) = replay {
            events.push(self.arguments_delta(index, &id, &arguments));
        }
        if stop {
            events.extend(self.step_stop(&json!({ "index": index })));
        }
        events
    }

    /// `interactionsFinishPatchCalls`: at the end of the response, settles
    /// each `apply_patch` call's IDs and stops it, in index order. A call
    /// never named fails the stream if the client declared `apply_patch`.
    pub(in super::super) fn finish_patch_calls(&mut self) -> Events {
        if self.patch_bridge && self.calls.values().any(|call| call.raw_name.is_empty()) {
            return self.patch_failure(UNRESOLVED);
        }
        let indexes: Vec<i64> = self
            .calls
            .iter()
            .filter(|(_, call)| {
                is_patch(&self.identities, &call.raw_name) && !call.item_done_emitted
            })
            .map(|(&index, _)| index)
            .collect();
        let mut events = Vec::new();
        for index in indexes {
            let Some(call) = self.calls.get_mut(&index) else {
                continue;
            };
            if call.patch.is_none() {
                // Interactions already maps an absent call_id to id (and an
                // absent id to call_id/item_<index>). Freeze that
                // compatibility mapping only at the response terminal, after
                // all supplied snapshot IDs were reconciled.
                if !call.item_id_seen {
                    call.id = first_non_empty([&call.call_id, &format!("item_{index}")]).to_owned();
                }
                if !call.call_id_seen {
                    call.call_id.clone_from(&call.id);
                }
                call.identity_finalized = true;
                events.extend(self.update_function_call(index, None, false));
                if self.terminal {
                    break;
                }
            }
            events.extend(self.step_stop(&json!({ "index": index })));
            if self.terminal {
                break;
            }
        }
        events
    }
}

/// Keeps the first error found for a call.
fn record(call: &mut Call, error: ToolInputError) {
    call.pending_error.get_or_insert(error);
}

/// `mergeID`: adopts an item ID (or, for `call_id`, a call ID) the step
/// gives, unless the call already has another one. `patch_name` is whether
/// the call's name so far is `apply_patch`'s.
fn merge_id(call: &mut Call, call_id: bool, value: &str, patch_name: bool) {
    if value.is_empty() {
        return;
    }
    let announced = call.added && !patch_name;
    let (target, seen) = if call_id {
        (&mut call.call_id, &mut call.call_id_seen)
    } else {
        (&mut call.id, &mut call.item_id_seen)
    };
    if (*seen || announced) && target != value {
        record(call, CONFLICTING_IDENTITY);
        return;
    }
    *target = value.to_owned();
    *seen = true;
}

/// Records a full snapshot of a call's arguments, checking it against the
/// snapshots before it and any input already completed.
fn snapshot(call: &mut Call, arguments: String) {
    // A later complete snapshot is not a new prefix for already buffered
    // fragments.
    if !call.added && call.initial_arguments.is_empty() && call.arguments.is_empty() {
        call.initial_arguments.clone_from(&arguments);
    }
    match CallState::default().finish_arguments(&arguments) {
        Err(error) => record(call, error.into()),
        Ok((_, input)) => {
            if call.has_snapshot && input != call.snapshot_input {
                record(call, CONFLICTING_SNAPSHOTS);
            }
            if call.item_done_emitted
                && call
                    .patch
                    .as_ref()
                    .is_some_and(|patch| input != patch.input())
            {
                record(call, SNAPSHOT_AFTER_DONE);
            }
            call.has_snapshot = true;
            call.snapshot_input = input;
            call.snapshot_arguments = arguments;
        }
    }
}
