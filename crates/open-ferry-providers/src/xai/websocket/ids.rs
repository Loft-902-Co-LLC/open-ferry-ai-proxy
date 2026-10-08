// Ported from CLIProxyAPI internal/runtime/executor/xai_websockets_executor.go
// (xaiWebsocketIDStateStore, xaiWebsocketIDState, xaiWebsocketRequestIDMapper,
// getXAIWebsocketIDState, deleteXAIWebsocketIDState,
// newXAIWebsocketRequestIDMapper, rewriteXAIWebsocketDownstreamIDs)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What a session remembers between its calls: the response IDs the client
//! was given for xAI's, and a transcript to replay when xAI can't continue
//! from a previous response.
//!
//! A session is the client's Responses WebSocket session, else the
//! `prompt_cache_key` it sent; a call with neither remembers nothing. The
//! calls of a `prompt_cache_key` outside a WebSocket session go one at a
//! time.
//!
//! xAI may answer two calls with the same response ID. The client gets
//! `<id>-xai-<n>` for a repeat (one already given, or the previous
//! response's own), and the IDs of the response's items change with it; a
//! call naming it as its `previous_response_id` sends xAI's again.
//!
//! The transcript is each call's input and its completed response's
//! output. It goes before the input of a call whose previous response xAI
//! no longer knows: one whose ID maps to nothing (after a compaction), or
//! any once the session's connection went to another credential, URL or
//! proxy. After a compaction it holds the compaction item alone, and goes
//! before the input of a `response.append` that names no previous response
//! too.
//!
//! Deviations from upstream:
//! - An event whose IDs change is written by `serde_json`, keeping its key
//!   order; Go writes it with sorted keys and HTML escaping.
//! - A payload that isn't a JSON object is read as an empty object.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::Value;
use tokio::sync::OwnedMutexGuard;

use crate::json::{delete, get, set, str_at};

/// What every session remembers (`xaiWebsocketIDStateStore`).
#[derive(Default)]
pub(super) struct Store {
    states: Mutex<HashMap<String, Arc<State>>>,
}

impl Store {
    fn states(&self) -> MutexGuard<'_, HashMap<String, Arc<State>>> {
        self.states.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What the session `id` names remembers, made if new; `None` for a
    /// blank ID (`getXAIWebsocketIDState`).
    pub(super) fn get(&self, id: &str) -> Option<Arc<State>> {
        let id = id.trim();
        if id.is_empty() {
            return None;
        }
        Some(Arc::clone(self.states().entry(id.to_owned()).or_default()))
    }

    /// Forgets the session `id` names (`deleteXAIWebsocketIDState`).
    pub(super) fn delete(&self, id: &str) {
        let id = id.trim();
        if !id.is_empty() {
            self.states().remove(id);
        }
    }
}

/// What one session remembers (`xaiWebsocketIDState`).
#[derive(Default)]
pub(super) struct State {
    /// Held by a call outside a WebSocket session for its whole stream.
    requests: Arc<tokio::sync::Mutex<()>>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// xAI's response ID for each the client was given.
    downstream_to_upstream: HashMap<String, String>,
    /// How many repeated IDs were renamed.
    sequence: u64,
    /// The input and output items so far.
    transcript: Vec<Value>,
    /// Whether the transcript is a compaction's, replayed for a
    /// `response.append` that names no previous response.
    replay_on_reset: bool,
}

impl State {
    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits for the session's call before to end (`requestMu`).
    pub(super) async fn lock_requests(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.requests).lock_owned().await
    }

    /// xAI's ID for `downstream`, which the client was given, or
    /// `downstream` itself (`upstreamIDForDownstream`).
    fn upstream_id_for(&self, downstream: &str) -> String {
        let downstream = downstream.trim();
        if downstream.is_empty() {
            return String::new();
        }
        match self.inner().downstream_to_upstream.get(downstream) {
            Some(upstream) => upstream.trim().to_owned(),
            None => downstream.to_owned(),
        }
    }

    /// Has `downstream` stand for xAI's `upstream`, empty for none
    /// (`mapDownstreamToUpstream`).
    pub(super) fn map_downstream_to_upstream(&self, downstream: &str, upstream: &str) {
        let downstream = downstream.trim();
        if downstream.is_empty() {
            return;
        }
        self.inner()
            .downstream_to_upstream
            .insert(downstream.to_owned(), upstream.trim().to_owned());
    }

    /// The transcript so far (`snapshotTranscriptInput`).
    pub(super) fn transcript(&self) -> Vec<Value> {
        self.inner().transcript.clone()
    }

    /// Puts the transcript before `body`'s input (`prependTranscriptInput`).
    fn prepend_transcript(&self, body: &mut Value) {
        let prefix = self.transcript();
        if !prefix.is_empty() {
            prepend_input(body, prefix);
        }
    }

    /// Puts a compaction's transcript before `body`'s input, if the
    /// transcript is one; returns whether it did
    /// (`prependCompactedTranscriptOnReset`).
    fn prepend_compacted_on_reset(&self, body: &mut Value) -> bool {
        let prefix = {
            let inner = self.inner();
            if !inner.replay_on_reset || inner.transcript.is_empty() {
                return false;
            }
            inner.transcript.clone()
        };
        prepend_input(body, prefix);
        true
    }

    /// Adds a call's input (in `request`, the message sent) and its
    /// response's output (in `completed`, an event) to the transcript,
    /// emptied first when `reset` (`recordTranscriptTurn`).
    pub(super) fn record_turn(&self, request: &Value, completed: &[u8], reset: bool) {
        let completed: Value = serde_json::from_slice(completed).unwrap_or(Value::Null);
        let input = items(get(request, "input"));
        let output = items(get(&completed, "response.output"));
        let mut inner = self.inner();
        if reset {
            inner.transcript.clear();
            inner.replay_on_reset = false;
        }
        inner.transcript.extend(input);
        inner.transcript.extend(output);
    }

    /// Makes `items` the transcript, a compaction's when there are any
    /// (`replaceTranscriptWithItems`).
    pub(super) fn replace_transcript(&self, items: Vec<Value>) {
        let mut inner = self.inner();
        inner.replay_on_reset = !items.is_empty();
        inner.transcript = items;
    }
}

/// The items of an array (`xaiJSONRawMessages`); none for anything else.
fn items(value: Option<&Value>) -> Vec<Value> {
    match value {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    }
}

/// Makes `body`'s input `prefix` followed by its own items.
fn prepend_input(body: &mut Value, mut prefix: Vec<Value>) {
    prefix.extend(items(get(body, "input")));
    set(body, "input", Value::Array(prefix));
}

/// The IDs of one call (`xaiWebsocketRequestIDMapper`).
pub(super) struct Mapper {
    state: Arc<State>,
    /// The client's `previous_response_id`, trimmed.
    downstream_previous_id: String,
    /// xAI's ID for it, empty when xAI no longer knows it.
    upstream_previous_id: String,
    /// xAI's ID for this response, once seen.
    upstream_response_id: String,
    /// The ID the client gets for it.
    downstream_response_id: String,
    /// Whether the transcript went before the input.
    replayed: bool,
}

impl Mapper {
    /// The mapper for a call of the session `state` belongs to, whose
    /// client payload is `client` (`newXAIWebsocketRequestIDMapper`).
    pub(super) fn new(state: Arc<State>, client: &Value) -> Self {
        let downstream_previous_id = str_at(client, "previous_response_id").trim().to_owned();
        let upstream_previous_id = state.upstream_id_for(&downstream_previous_id);
        Self {
            state,
            downstream_previous_id,
            upstream_previous_id,
            upstream_response_id: String::new(),
            downstream_response_id: String::new(),
            replayed: false,
        }
    }

    pub(super) fn state(&self) -> &State {
        &self.state
    }

    /// Whether the transcript went before the input.
    pub(super) fn replayed(&self) -> bool {
        self.replayed
    }

    /// xAI's ID for the client's previous response, empty for none.
    pub(super) fn upstream_previous_id(&self) -> &str {
        &self.upstream_previous_id
    }

    /// Forgets xAI's ID for the previous response, which the connection's
    /// new target doesn't know.
    pub(super) fn forget_upstream_previous(&mut self) {
        self.upstream_previous_id.clear();
    }

    /// Rewrites the body sent to xAI (`upstreamRequestPayload`): xAI's ID
    /// for the previous response, or none and the transcript before the
    /// input when xAI no longer knows it. A `response.append` naming no
    /// previous response gets a compaction's transcript.
    pub(super) fn upstream_request(&mut self, body: &mut Value) {
        if self.downstream_previous_id == self.upstream_previous_id {
            if self.downstream_previous_id.is_empty()
                && str_at(body, "type").trim() == "response.append"
            {
                self.replayed = self.state.prepend_compacted_on_reset(body);
            }
            return;
        }
        if self.upstream_previous_id.is_empty() {
            delete(body, "previous_response_id");
            if !self.downstream_previous_id.is_empty() {
                self.state.prepend_transcript(body);
                self.replayed = true;
            }
            return;
        }
        set(
            body,
            "previous_response_id",
            Value::from(self.upstream_previous_id.as_str()),
        );
    }

    /// Rewrites an event for the client (`downstreamResponsePayload`): the
    /// response's ID and its items' IDs, and the previous response's.
    pub(super) fn downstream_response(&mut self, payload: Vec<u8>) -> Vec<u8> {
        if payload.is_empty() {
            return payload;
        }
        let Ok(mut event) = serde_json::from_slice::<Value>(&payload) else {
            return payload;
        };
        let upstream = str_at(&event, "response.id");
        let downstream = self.downstream_id_for(&upstream);
        if downstream.is_empty() {
            return payload;
        }
        let ids = Ids {
            upstream_response: self.upstream_response_id.trim(),
            downstream_response: downstream.trim(),
            upstream_previous: self.upstream_previous_id.trim(),
            downstream_previous: self.downstream_previous_id.trim(),
        };
        if ids.upstream_response == ids.downstream_response
            && ids.upstream_previous == ids.downstream_previous
        {
            return payload;
        }
        if !rewrite(&mut event, &ids) {
            return payload;
        }
        event.to_string().into_bytes()
    }

    /// The ID the client gets for xAI's response `upstream`
    /// (`downstreamIDForUpstreamResponse`): its own, or a new one if the
    /// client was given it already or it is the previous response's.
    fn downstream_id_for(&mut self, upstream: &str) -> String {
        if !self.upstream_response_id.is_empty() {
            return self.downstream_response_id.clone();
        }
        let upstream = upstream.trim();
        if upstream.is_empty() {
            return String::new();
        }
        let mut inner = self.state.inner();
        self.upstream_response_id = upstream.to_owned();
        self.downstream_response_id = upstream.to_owned();
        let seen = inner.downstream_to_upstream.contains_key(upstream);
        let repeats_previous = !self.downstream_previous_id.is_empty()
            && !self.upstream_previous_id.is_empty()
            && upstream == self.upstream_previous_id;
        if repeats_previous || seen {
            inner.sequence += 1;
            self.downstream_response_id = format!("{upstream}-xai-{}", inner.sequence);
        }
        inner
            .downstream_to_upstream
            .insert(upstream.to_owned(), upstream.to_owned());
        inner
            .downstream_to_upstream
            .insert(self.downstream_response_id.clone(), upstream.to_owned());
        self.downstream_response_id.clone()
    }
}

/// The IDs an event's are rewritten from and to.
struct Ids<'a> {
    upstream_response: &'a str,
    downstream_response: &'a str,
    upstream_previous: &'a str,
    downstream_previous: &'a str,
}

impl Ids<'_> {
    /// The string under `key` rewritten, or `None` if it stays
    /// (`rewriteXAIWebsocketDownstreamIDString`): an `id` or `item_id`
    /// holding xAI's response ID, or a `previous_response_id` that is xAI's
    /// previous one.
    fn rewrite(&self, value: &str, key: &str) -> Option<String> {
        let replaced = match key {
            "id" | "item_id"
                if !self.upstream_response.is_empty()
                    && !self.downstream_response.is_empty()
                    && self.downstream_response != self.upstream_response
                    && value.contains(self.upstream_response) =>
            {
                value.replace(self.upstream_response, self.downstream_response)
            }
            "previous_response_id"
                if !self.upstream_previous.is_empty()
                    && !self.downstream_previous.is_empty()
                    && value == self.upstream_previous =>
            {
                self.downstream_previous.to_owned()
            }
            _ => return None,
        };
        (replaced != value).then_some(replaced)
    }
}

/// Rewrites the IDs in `value`; returns whether any changed
/// (`rewriteXAIWebsocketDownstreamIDValue`). A string in an array stays.
fn rewrite(value: &mut Value, ids: &Ids<'_>) -> bool {
    match value {
        Value::Object(fields) => {
            let mut changed = false;
            for (child_key, child) in fields.iter_mut() {
                if let Value::String(text) = child {
                    if let Some(replaced) = ids.rewrite(text, child_key) {
                        *text = replaced;
                        changed = true;
                    }
                    continue;
                }
                changed |= rewrite(child, ids);
            }
            changed
        }
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= rewrite(item, ids);
            }
            changed
        }
        _ => false,
    }
}
