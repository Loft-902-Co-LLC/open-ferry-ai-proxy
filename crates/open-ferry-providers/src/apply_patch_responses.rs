// Ported from CLIProxyAPI internal/runtime/executor/helps/apply_patch_responses.go
// and helps/apply_patch.go (ApplyPatchRequested, InitializeApplyPatchStream)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The `apply_patch` bridge as an executor runs it, for a provider that
//! speaks the Responses API but only knows function tools.
//!
//! An executor opts in explicitly: it rewrites the request with
//! [`normalize_request`], builds a [`State`] for the response, and passes
//! each line of the provider's stream through [`State::stream`] (or each
//! event without its SSE framing through [`State::transform`]) before
//! translating it. Nothing turns the bridge on from the wire format or from a
//! function the client declared. Upstream's xAI, Meta and Kimi executors do
//! this; its Codex executor doesn't.
//!
//! Beyond the translator's [`Bridge`], the state checks that the response
//! really ended: a stream that stops, or sends `[DONE]`, before a JSON
//! terminal event fails with one `response.failed` event. It also undoes a
//! namespace dispatcher. xAI calls a namespace's children through one
//! function named after the namespace, whose arguments wrap the child's:
//! `{"name":"apply_patch","arguments":{"input":"…"}}`. The executor declares
//! such a dispatcher with [`State::add_dispatcher`], and the state holds its
//! events until it knows which child was called, then restores the child's
//! name, namespace and arguments. A patch child whose evidence disagrees
//! anywhere fails the stream.
//!
//! An executor that translates another format's stream to Responses readies
//! the translator with `initialize_stream`, so that a stream that ends
//! before sending anything still fails a requested patch.
//!
//! Deviations from upstream:
//! - An event the state changes or makes, such as a restored dispatcher call,
//!   is written as compact JSON. Upstream edits the event's original text.
//!   Setting a field to the value it already holds changes nothing.
//! - An event that isn't JSON is read as having no fields. gjson reads what
//!   it can from it. Such an event never belongs to a call, so it passes to
//!   the [`Bridge`] as it is.
//! - Of an event's repeated keys, the last counts; gjson takes the first.
//!   Dispatcher arguments are read as gjson reads them, malformed or not,
//!   except that text that doesn't start with an object has no fields, where
//!   gjson may still find some in it.
//! - What the state holds for dispatcher calls is bounded: the bytes of their
//!   events, snapshots, streamed arguments and keys, the events and snapshots
//!   themselves, and the calls. Upstream keeps all of it until the response
//!   ends, so a stream of events that never completes a call, even empty ones,
//!   grows without end whatever the limit on each line. Past a limit the
//!   response fails as any other conflict does, with one `response.failed`
//!   event, at the next event or at the end of the stream. The limits are
//!   200 MiB, 262,144 events and 16,384 calls.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::mem;

use open_ferry_core::exec::Format;
use open_ferry_translate::apply_patch::responses::{
    self, Bridge, Error, LenientValue, lenient_get, qualify_tool_name,
};
use open_ferry_translate::apply_patch::{is_custom_tool, unwrap_input};
use open_ferry_translate::go;
use open_ferry_translate::registry::ResponseStream;
use serde_json::{Value, json};

use crate::codex::stream::MAX_LINE;
use crate::json::{get, int_of, str_at, str_of};

const ADDED: &str = "response.output_item.added";
const ITEM_DONE: &str = "response.output_item.done";
const DELTA: &str = "response.function_call_arguments.delta";
const ARGUMENTS_DONE: &str = "response.function_call_arguments.done";

/// What the state may hold for the dispatcher calls of one response. Not
/// upstream's: see the module's deviations.
///
/// The bytes count each buffer's contents and a fixed `ENTRY_COST` for the
/// buffer itself, so that events with nothing in them still add up. A single
/// line can be [`MAX_LINE`] long, and the state keeps up to three copies of
/// an event (`events`, `originals` and `snapshots`), so the byte limit lets
/// the largest line through in all of them, with room for the rest of the
/// call. The event limit is twice the events of a call that streams 128K
/// tokens, one delta event each. The call limit is far above the calls one
/// response makes.
#[derive(Clone, Copy, Debug)]
struct Limits {
    /// The bytes held.
    bytes: usize,
    /// The events and snapshots held. The provider's text of each event is
    /// kept with it and not counted again.
    events: usize,
    /// The calls recorded, whether or not they turn out to be dispatchers'.
    calls: usize,
}

impl Limits {
    const DEFAULT: Self = Self {
        bytes: 4 * MAX_LINE,
        events: 1 << 18,
        calls: 1 << 14,
    };
}

/// What one buffer costs beyond its contents: its header, and the allocator's
/// slack.
const ENTRY_COST: usize = 64;

/// What one recorded call costs beyond what it holds.
const CALL_COST: usize = mem::size_of::<DispatcherCall>();

/// The text of the error when a limit is passed.
const LIMIT_MESSAGE: &str = "apply_patch dispatcher buffering limit exceeded";

fn limit_error() -> Error {
    Error::new(LIMIT_MESSAGE)
}

/// What a list of buffers costs.
fn held_size(buffers: &[Vec<u8>]) -> usize {
    buffers.iter().map(|buffer| buffer.len() + ENTRY_COST).sum()
}

/// `ApplyPatchRequested`: whether a custom `apply_patch` tool wins its name
/// in the client's `original` request. As upstream reads the request's tools
/// for Gemini (`ResponsesToolReverseIdentityMap`), a request wrapped in a
/// `request` that holds a model, input or tools is read from inside it.
pub(crate) fn requested(original: &Value) -> bool {
    let mut root = original;
    if let Some(request) = original.get("request")
        && ["model", "input", "tools"]
            .iter()
            .any(|key| request.get(*key).is_some())
    {
        root = request;
    }
    Bridge::new(root).active()
}

/// `InitializeApplyPatchStream`: readies `translator`, which translates a
/// response to the client's `original` request into `response_format`, by
/// giving it an empty chunk, if the client gets Responses events and
/// [`requested`] a patch. A translator that never saw a chunk has nothing to
/// finish, so without this a stream that ends before sending anything would
/// end without failing the patch. What the empty chunk gives is dropped, as
/// upstream drops it.
pub(crate) fn initialize_stream(
    translator: &mut ResponseStream,
    response_format: &Format,
    original: &Value,
) {
    if *response_format != Format::OPENAI_RESPONSE || !requested(original) {
        return;
    }
    let _ = translator.translate(b"");
}

/// `NormalizeApplyPatchResponsesRequest`: opts a request to a provider that
/// only knows function tools into the patch contract.
///
/// Given the client's `original` request, a custom `apply_patch` tool that a
/// Chat Completions function of the same name also declares is dropped
/// first, so the function keeps winning as it does in the Chat request
/// converter. Then [`responses::normalize_request`] declares the custom tool
/// as a function. On an error, only the first step's change has been made.
pub fn normalize_request(body: &mut Value, original: Option<&Value>) -> Result<(), Error> {
    if let Some(original) = original {
        prefer_chat_function_patch_tools(original, body);
    }
    responses::normalize_request(body)
}

/// `preferChatFunctionPatchTools`: drops a custom `apply_patch` declaration
/// that a function of the same name also declares, if the client's Chat
/// request declared that function. Once the Chat request declares any
/// function, `tools` is written back even if nothing was dropped.
fn prefer_chat_function_patch_tools(original: &Value, declarations: &mut Value) {
    let ordinary: HashSet<String> = items(get(original, "tools"))
        .into_iter()
        .filter(|tool| str_at(tool, "type") == "function")
        .map(|tool| str_at(tool, "function.name"))
        .collect();
    if ordinary.is_empty() {
        return;
    }
    let declared = items(get(declarations, "tools"));
    let available: HashSet<String> = declared
        .iter()
        .filter(|tool| str_at(tool, "type") == "function")
        .map(|tool| str_at(tool, "name"))
        .collect();
    let tools: Vec<Value> = declared
        .into_iter()
        .filter(|tool| {
            let name = str_at(tool, "name");
            !(is_custom_tool(tool) && ordinary.contains(&name) && available.contains(&name))
        })
        .cloned()
        .collect();
    crate::json::set(declarations, "tools", Value::Array(tools));
}

/// `ApplyPatchResponsesState`: the bridge for one response, owned by the
/// executor serving it.
pub struct State {
    /// The translator's bridge, which converts the patch calls.
    pub bridge: Bridge,
    /// Each dispatcher function's name, and the namespace it calls into.
    dispatchers: HashMap<String, String>,
    /// The call each `item:`, `call:` or `index:` key names, in `records`.
    by_key: HashMap<String, usize>,
    /// Each call that may be to a dispatcher, in the order first seen.
    records: Vec<DispatcherCall>,
    /// The provider's own text of the next event, before the executor
    /// restored namespaces in it.
    upstream: Option<Vec<u8>>,
    /// An `event:` line waiting for its `data:` line.
    event_line: Option<Vec<u8>>,
    active: bool,
    failed: bool,
    /// Whether a JSON terminal event has ended the response.
    closed: bool,
    /// Whether `[DONE]` has ended the stream.
    transport_done: bool,
    /// What may be held.
    limits: Limits,
    /// The bytes held in `by_key` and `records`, as [`Limits`] counts them.
    held_bytes: usize,
    /// The events and snapshots held in `records`.
    held_events: usize,
    /// Whether the executor handed over more than may be held through
    /// [`remember_dispatcher_event`](Self::remember_dispatcher_event) or
    /// [`remember_dispatcher_arguments`](Self::remember_dispatcher_arguments),
    /// which can't report it. The next event, or the end of the stream, fails.
    overflowed: bool,
}

/// `patchDispatcherCall`: one call that may be to a dispatcher.
struct DispatcherCall {
    /// The namespace, once a dispatcher's name proves the call is one.
    namespace: String,
    /// Its events held back, as restored so far.
    events: Vec<Vec<u8>>,
    /// The provider's `function_call_arguments.done` events for it.
    snapshots: Vec<Vec<u8>>,
    /// Its streamed arguments.
    source: String,
    /// The provider's text of each of `events`.
    originals: Vec<Vec<u8>>,
    completed: bool,
    /// Whether it turned out to call an ordinary function.
    ordinary: bool,
    /// Its output index, or -1 until one is seen.
    index: i64,
    /// The child's name and arguments, once completed.
    name: String,
    arguments: String,
}

impl Default for DispatcherCall {
    fn default() -> Self {
        Self {
            namespace: String::new(),
            events: Vec::new(),
            snapshots: Vec::new(),
            source: String::new(),
            originals: Vec::new(),
            completed: false,
            ordinary: false,
            index: -1,
            name: String::new(),
            arguments: String::new(),
        }
    }
}

impl State {
    /// `NewApplyPatchResponsesState`: the state for a response to a request
    /// whose tools `declarations` holds, before [`normalize_request`].
    /// `original` is the client's request, in the `source` format; a Chat
    /// Completions request's functions win over a custom `apply_patch` as in
    /// [`normalize_request`]. It is active if a custom `apply_patch` tool
    /// wins its name.
    pub fn new(source: &Format, original: &Value, declarations: &Value) -> Self {
        let mut declarations = Cow::Borrowed(declarations);
        if *source == Format::OPENAI {
            prefer_chat_function_patch_tools(original, declarations.to_mut());
        }
        let bridge = Bridge::new(&declarations);
        let active = bridge.active();
        Self {
            bridge,
            dispatchers: HashMap::new(),
            by_key: HashMap::new(),
            records: Vec::new(),
            upstream: None,
            event_line: None,
            active,
            failed: false,
            closed: false,
            transport_done: false,
            limits: Limits::DEFAULT,
            held_bytes: 0,
            held_events: 0,
            overflowed: false,
        }
    }

    /// `Active`: whether the request declares a winning custom `apply_patch`
    /// tool. An inactive state passes everything through.
    pub fn active(&self) -> bool {
        self.active
    }

    /// `AddDispatcher`: declares the function `name` as the dispatcher of
    /// `namespace`, if a custom `apply_patch` tool wins a name in that
    /// namespace. Otherwise the function stays ordinary.
    pub fn add_dispatcher(&mut self, name: &str, namespace: &str) {
        let patches = self
            .bridge
            .tools()
            .any(|tool| tool.namespace == namespace && tool.apply_patch);
        if patches {
            self.dispatchers
                .insert(name.to_owned(), namespace.to_owned());
        }
    }

    /// The namespace a dispatcher calls into, or `""` if `name` isn't one.
    fn dispatcher_namespace(&self, name: &str) -> &str {
        self.dispatchers.get(name).map_or("", String::as_str)
    }

    /// Whether the tool `name` in `namespace` is the custom `apply_patch`.
    fn is_patch(&self, namespace: &str, name: &str) -> bool {
        self.bridge
            .tool(namespace, name)
            .is_some_and(|tool| tool.apply_patch)
    }

    /// `dispatcher`: the first call any key of the event names.
    fn dispatcher(&self, root: &Value) -> Option<usize> {
        dispatcher_keys(root)
            .iter()
            .filter_map(|key| self.by_key.get(key).copied())
            .min()
    }

    /// Counts `bytes` and `events` more held, or fails if that passes a limit.
    fn hold(&mut self, bytes: usize, events: usize) -> Result<(), Error> {
        let bytes = self.held_bytes.saturating_add(bytes);
        let events = self.held_events.saturating_add(events);
        if bytes > self.limits.bytes || events > self.limits.events {
            return Err(limit_error());
        }
        self.held_bytes = bytes;
        self.held_events = events;
        Ok(())
    }

    /// Counts `bytes` and `events` fewer held.
    fn release(&mut self, bytes: usize, events: usize) {
        self.held_bytes = self.held_bytes.saturating_sub(bytes);
        self.held_events = self.held_events.saturating_sub(events);
    }

    /// Counts a held buffer of `old` bytes as `new` bytes, failing if that
    /// passes a limit.
    fn resize(&mut self, old: usize, new: usize) -> Result<(), Error> {
        if new > old {
            self.hold(new - old, 0)
        } else {
            self.release(old - new, 0);
            Ok(())
        }
    }

    /// Fails if the state holds more than the executor was allowed to hand it.
    fn check_overflow(&self) -> Result<(), Error> {
        if self.overflowed {
            Err(limit_error())
        } else {
            Ok(())
        }
    }

    /// Names `call` by the event's keys that no call holds yet.
    fn index_keys(&mut self, root: &Value, call: usize) -> Result<(), Error> {
        for key in dispatcher_keys(root) {
            if !self.by_key.contains_key(&key) {
                self.hold(key.len() + ENTRY_COST, 0)?;
                self.by_key.insert(key, call);
            }
        }
        Ok(())
    }

    /// `newDispatcherCandidate`: records a new call, under the event's keys
    /// that no call holds yet.
    fn new_candidate(&mut self, root: &Value) -> Result<usize, Error> {
        let call = self.records.len();
        if call >= self.limits.calls {
            return Err(limit_error());
        }
        self.hold(CALL_COST, 0)?;
        self.records.push(DispatcherCall::default());
        self.index_keys(root, call)?;
        Ok(call)
    }

    /// `RememberDispatcherEvent`: keeps the provider's text of the event the
    /// executor is about to restore namespaces in and pass to
    /// [`transform`](Self::transform). Only once a dispatcher is declared.
    pub fn remember_dispatcher_event(&mut self, event: &[u8]) {
        if self.failed || self.closed || self.transport_done || self.dispatchers.is_empty() {
            return;
        }
        self.upstream = Some(event.to_vec());
        self.remember_dispatcher_arguments(event);
    }

    /// `RememberDispatcherArguments`: keeps a provider's
    /// `function_call_arguments.done` event on every call its keys name,
    /// recording a new call if none. A wrapper in the arguments proves
    /// nothing by itself; only a dispatcher's name does.
    ///
    /// Past a limit, the next event or the end of the stream fails.
    pub fn remember_dispatcher_arguments(&mut self, event: &[u8]) {
        if self.failed
            || self.closed
            || self.transport_done
            || self.overflowed
            || self.dispatchers.is_empty()
        {
            return;
        }
        let root = parse(event);
        if str_at(&root, "type") != ARGUMENTS_DONE {
            return;
        }
        self.upstream = Some(event.to_vec());
        if self.remember_snapshot(&root, event).is_err() {
            self.overflowed = true;
        }
    }

    /// Keeps `event`, the `function_call_arguments.done` event `root`, on
    /// every call its keys name.
    fn remember_snapshot(&mut self, root: &Value, event: &[u8]) -> Result<(), Error> {
        if self.dispatcher(root).is_none() {
            self.new_candidate(root)?;
        }
        let mut seen = HashSet::new();
        for key in dispatcher_keys(root) {
            if let Some(&call) = self.by_key.get(&key)
                && seen.insert(call)
            {
                self.hold(event.len() + ENTRY_COST, 1)?;
                self.records[call].snapshots.push(event.to_vec());
            }
        }
        Ok(())
    }

    /// `expandDispatcher`: holds a dispatcher call's events until its child
    /// is known, then returns them restored. `original` is the provider's
    /// text of `event`.
    fn expand_dispatcher(
        &mut self,
        event: Vec<u8>,
        original: &[u8],
    ) -> Result<Vec<Vec<u8>>, Error> {
        let root = parse(&event);
        let raw = parse(original);
        let kind = str_at(&root, "type");
        let mut call = self.dispatcher(&root);
        let declared = self.dispatchers.get(&dispatcher_event_name(&raw)).cloned();
        if call.is_none() && !self.dispatchers.is_empty() {
            let function_call = str_at(&raw, "item.type") == "function_call";
            let added = kind == ADDED && function_call;
            if (declared.is_some() && function_call)
                || added
                || kind == DELTA
                || kind == ARGUMENTS_DONE
            {
                call = Some(self.new_candidate(&root)?);
            }
        }
        let Some(call) = call else {
            return Ok(vec![event]);
        };
        self.bridge.check_identity(&event)?;
        self.index_keys(&root, call)?;
        let record = &mut self.records[call];
        if record.index < 0
            && let Some(index) = get(&root, "output_index")
        {
            record.index = int_of(Some(index));
        }
        if record.ordinary && declared.is_none() {
            return Ok(vec![event]);
        }
        self.hold(event.len() + original.len() + 2 * ENTRY_COST, 1)?;
        let record = &mut self.records[call];
        record.events.push(event.clone());
        record.originals.push(original.to_vec());
        if let Some(namespace) = &declared {
            if !record.namespace.is_empty() && record.namespace != *namespace {
                return Err(Error::new("conflicting apply_patch dispatcher namespace"));
            }
            record.namespace.clone_from(namespace);
            record.ordinary = false;
        }
        if kind == DELTA {
            let delta = str_at(&root, "delta");
            if record.completed && !delta.is_empty() {
                let (namespace, child) = (record.namespace.clone(), record.name.clone());
                if self.is_patch(&namespace, &child) {
                    return Err(Error::new(
                        "apply_patch dispatcher arguments received after completion",
                    ));
                }
                return Ok(vec![event]);
            }
            self.hold(delta.len(), 0)?;
            self.records[call].source.push_str(&delta);
        }
        let record = &mut self.records[call];
        if record.namespace.is_empty() {
            if !dispatcher_event_name(&raw).is_empty() {
                // A late ordinary name releases untouched arguments, even if
                // they look like a wrapper.
                record.ordinary = true;
                let events = mem::take(&mut record.events);
                let originals = mem::take(&mut record.originals);
                self.release(held_size(&events) + held_size(&originals), events.len());
                return Ok(events);
            }
            return Ok(Vec::new());
        }
        if kind == DELTA && !record.completed {
            return Ok(Vec::new());
        }
        let progress =
            record.completed && (kind == ARGUMENTS_DONE || kind == DELTA || kind == ADDED);
        if kind != ITEM_DONE && !progress {
            return Ok(Vec::new());
        }
        let path = if kind == ARGUMENTS_DONE || kind == DELTA {
            ""
        } else {
            "item."
        };

        let record = &self.records[call];
        let namespace = record.namespace.clone();
        let mut wrappers: Vec<String> = Vec::new();
        if !record.source.is_empty() {
            wrappers.push(record.source.clone());
        }
        for snapshot in &record.snapshots {
            let arguments = str_at(&parse(snapshot), "arguments");
            if lenient_get(&arguments, "name").is_some() {
                wrappers.push(arguments);
            }
        }
        for pending in &record.originals {
            let pending = parse(pending);
            let arguments = str_at(&pending, "item.arguments");
            let pending_name = dispatcher_event_name(&pending);
            let ours =
                pending_name.is_empty() || self.dispatcher_namespace(&pending_name) == namespace;
            if ours && lenient_get(&arguments, "name").is_some() {
                wrappers.push(arguments);
            }
        }
        // For callers without the provider's text, the events' own
        // arguments count.
        if wrappers.is_empty() {
            for pending in &record.events {
                let arguments = str_at(&parse(pending), "arguments");
                if lenient_get(&arguments, "name").is_some_and(|name| !name.string.is_empty()) {
                    wrappers.push(arguments);
                }
            }
        }
        let mut source: Option<&str> = None;
        for wrapper in &wrappers {
            if go::gjson_valid(wrapper.as_bytes()) && !field(Some(wrapper), "name").is_empty() {
                source = Some(wrapper);
            }
        }

        let mut updated = root.clone();
        let mut changed = false;
        let mut name = str_at(&root, &format!("{path}name"));
        if name.is_empty() || self.dispatcher_namespace(&name) == namespace {
            name = field(source, "name");
            if name.is_empty() {
                name.clone_from(&record.name);
            }
            changed |= set(&mut updated, &format!("{path}name"), name.as_str().into());
            changed |= set(
                &mut updated,
                &format!("{path}namespace"),
                namespace.as_str().into(),
            );
        }
        if str_at(&root, &format!("{path}namespace")).is_empty() {
            changed |= set(
                &mut updated,
                &format!("{path}namespace"),
                namespace.as_str().into(),
            );
        }
        if declared.is_some() || dispatcher_event_name(&raw).is_empty() || kind == ARGUMENTS_DONE {
            let arguments = str_at(&raw, &format!("{path}arguments"));
            if !field(Some(&arguments), "name").is_empty() {
                let unwrapped = dispatcher_arguments(parsed_get(&arguments, "arguments"));
                changed |= set(&mut updated, &format!("{path}arguments"), unwrapped.into());
            }
        }
        if get(&root, &format!("{path}arguments")).is_none() {
            let mut encoded =
                dispatcher_arguments(source.and_then(|source| parsed_get(source, "arguments")));
            if encoded.is_empty() {
                encoded.clone_from(&record.arguments);
            }
            if !encoded.is_empty() {
                changed |= set(&mut updated, &format!("{path}arguments"), encoded.into());
            }
        }
        let root = updated;
        let event = if changed { to_bytes(&root) } else { event };

        let tool = self.bridge.tool(&namespace, &name);
        let patch = tool.is_some_and(|tool| tool.apply_patch);
        let qualified = tool.map(|tool| tool.name.clone()).unwrap_or_default();
        let mut final_arguments = str_at(&root, &format!("{path}arguments"));
        if kind == ADDED && final_arguments.is_empty() {
            final_arguments.clone_from(&record.arguments);
        }
        if patch {
            for snapshot in &record.snapshots {
                if !parse(snapshot)
                    .get("arguments")
                    .is_some_and(Value::is_string)
                {
                    return Err(Error::new(
                        "apply_patch dispatcher arguments snapshot must be a string",
                    ));
                }
            }
        }
        for wrapper in &wrappers {
            let wrapper_name = field(Some(wrapper), "name");
            if !patch && !self.is_patch(&namespace, &wrapper_name) {
                continue;
            }
            let input = unwrap_input(&dispatcher_arguments(parsed_get(wrapper, "arguments")));
            let final_input = unwrap_input(&final_arguments);
            if !go::gjson_valid(wrapper.as_bytes())
                || wrapper_name != name
                || input.is_none()
                || final_input.is_none()
                || input != final_input
            {
                return Err(Error::new("conflicting apply_patch dispatcher arguments"));
            }
        }

        // Completed aliases and source evidence stay until the response
        // closes. A repeated snapshot checks only the new event, and replays
        // nothing already sent.
        let replaced = self.records[call].events.last().map_or(0, Vec::len);
        self.resize(replaced, event.len())?;
        let record = &mut self.records[call];
        if let Some(last) = record.events.last_mut() {
            *last = event;
        }
        let start = if record.completed {
            record.events.len() - 1
        } else {
            0
        };
        let record = &self.records[call];
        let mut out = Vec::new();
        for (pending, original) in record.events[start..]
            .iter()
            .zip(&record.originals[start..])
        {
            let p = parse(pending);
            let original_root = parse(original);
            if patch {
                for namespace_path in ["namespace", "item.namespace"] {
                    let supplied = str_at(&original_root, namespace_path);
                    if !supplied.is_empty() && supplied != namespace {
                        return Err(Error::new("conflicting apply_patch dispatcher namespace"));
                    }
                }
                for supplied in [
                    dispatcher_event_name(&original_root),
                    dispatcher_event_name(&p),
                ] {
                    if !supplied.is_empty()
                        && self.dispatcher_namespace(&supplied) != namespace
                        && qualify_tool_name(&namespace, &supplied) != qualified
                    {
                        return Err(Error::new("conflicting apply_patch dispatcher child"));
                    }
                }
            }
            let pending_path = match str_at(&p, "type").as_str() {
                // A dispatcher's envelope is not the child's input streaming.
                DELTA if patch => continue,
                DELTA => {
                    out.push(pending.clone());
                    continue;
                }
                ADDED | ITEM_DONE => "item.",
                ARGUMENTS_DONE => "",
                _ => {
                    out.push(pending.clone());
                    continue;
                }
            };
            let mut updated = p.clone();
            let mut changed = false;
            let pending_name = str_at(&p, &format!("{pending_path}name"));
            if pending_name.is_empty() || self.dispatcher_namespace(&pending_name) == namespace {
                changed |= set(
                    &mut updated,
                    &format!("{pending_path}name"),
                    name.as_str().into(),
                );
                changed |= set(
                    &mut updated,
                    &format!("{pending_path}namespace"),
                    namespace.as_str().into(),
                );
            }
            if str_at(&p, &format!("{pending_path}namespace")).is_empty() {
                changed |= set(
                    &mut updated,
                    &format!("{pending_path}namespace"),
                    namespace.as_str().into(),
                );
            }
            // Only the provider's own wrappers are unwrapped, not a restored
            // child's arguments.
            let arguments = get(&original_root, &format!("{pending_path}arguments"));
            if patch && arguments.is_some_and(|arguments| !arguments.is_string()) {
                return Err(Error::new(
                    "apply_patch dispatcher arguments snapshot must be a string",
                ));
            }
            let arguments = str_of(arguments);
            let original_name = dispatcher_event_name(&original_root);
            let wrapped = original_name.is_empty()
                || self.dispatcher_namespace(&original_name) == namespace
                || pending_path.is_empty();
            if !arguments.is_empty() && wrapped {
                let wrapper_name = field(Some(&arguments), "name");
                if !wrapper_name.is_empty() {
                    if patch && wrapper_name != name {
                        return Err(Error::new("conflicting apply_patch dispatcher snapshot"));
                    }
                    let unwrapped = dispatcher_arguments(parsed_get(&arguments, "arguments"));
                    changed |= set(
                        &mut updated,
                        &format!("{pending_path}arguments"),
                        unwrapped.into(),
                    );
                }
            }
            out.push(if changed {
                to_bytes(&updated)
            } else {
                pending.clone()
            });
        }
        let record = &self.records[call];
        let replaced = record.name.len() + record.arguments.len();
        self.resize(replaced, name.len() + final_arguments.len())?;
        let record = &mut self.records[call];
        record.completed = true;
        record.name = name;
        record.arguments = final_arguments;
        Ok(out)
    }

    /// `unfinishedDispatcher`: whether a proven dispatcher call never
    /// completed.
    fn unfinished_dispatcher(&self) -> bool {
        self.records
            .iter()
            .any(|call| !call.namespace.is_empty() && !call.completed)
    }

    /// Drops the held calls and evidence.
    fn clear(&mut self) {
        self.by_key.clear();
        self.records.clear();
        self.upstream = None;
        self.held_bytes = 0;
        self.held_events = 0;
    }

    /// `fail`: ends the response with the bridge's one `response.failed`
    /// event.
    fn fail(&mut self, error: Error) -> (Vec<Vec<u8>>, Option<Error>) {
        if self.failed {
            return (Vec::new(), Some(error));
        }
        self.failed = true;
        self.clear();
        self.bridge.fail(error)
    }

    /// `Transform`: converts one event, without SSE framing. Returns the
    /// events to send, and the error once the stream fails. `[DONE]` passes
    /// only if the response has ended. Nothing passes after it, after a
    /// failure, or after a JSON terminal event.
    pub fn transform(&mut self, event: &[u8]) -> (Vec<Vec<u8>>, Option<Error>) {
        if self.failed || self.transport_done {
            return (Vec::new(), None);
        }
        if let Err(error) = self.check_overflow() {
            return self.fail(error);
        }
        if self.active && go::trim_space(event) == b"[DONE]" {
            if let Err(error) = self.finish() {
                return self.fail(error);
            }
            self.transport_done = true;
            return (vec![event.to_vec()], None);
        }
        if self.closed {
            return (Vec::new(), None);
        }
        let original = self.upstream.take().unwrap_or_else(|| event.to_vec());
        let root = parse(event);
        let kind = str_at(&root, "type");
        let terminal = matches!(
            kind.as_str(),
            "response.completed" | "response.incomplete" | "response.done"
        );
        let mut event = event.to_vec();
        let mut preceding = Vec::new();
        if terminal {
            let original_root = parse(&original);
            let original_items = items(get(&original_root, "response.output"));
            let mut updated = root.clone();
            let mut changed = false;
            for (i, item) in items(get(&root, "response.output")).into_iter().enumerate() {
                let mut done = json!({"type": ITEM_DONE, "item": item.clone()});
                // Explicit IDs come before a position in a sparse terminal
                // snapshot.
                let mut call = self.dispatcher(&done);
                let (id, call_id) = (str_at(item, "id"), str_at(item, "call_id"));
                if call.is_none() && id.is_empty() && call_id.is_empty() {
                    set(&mut done, "output_index", i.into());
                    call = self.dispatcher(&done);
                }
                let Some(call) = call.filter(|&call| !self.records[call].ordinary) else {
                    continue;
                };
                let index = match self.records[call].index {
                    index if index >= 0 => Value::from(index),
                    _ => Value::from(i),
                };
                set(&mut done, "output_index", index);
                // Filtering may shift positions, but restoring keeps both IDs.
                let matches = |candidate: &Value| {
                    str_at(candidate, "id") == id && str_at(candidate, "call_id") == call_id
                };
                let mut original_done = done.clone();
                if let Some(candidate) = original_items.get(i).filter(|item| matches(item)) {
                    set(&mut original_done, "item", Value::clone(candidate));
                } else if (!id.is_empty() || !call_id.is_empty())
                    && let Some(candidate) = original_items.iter().find(|item| matches(item))
                {
                    set(&mut original_done, "item", Value::clone(candidate));
                }
                let events =
                    match self.expand_dispatcher(to_bytes(&done), &to_bytes(&original_done)) {
                        Ok(events) => events,
                        Err(error) => return self.fail(error),
                    };
                let restored = events
                    .last()
                    .and_then(|last| parse(last).get_mut("item").map(Value::take));
                if let Some(restored) = restored {
                    changed |= set(&mut updated, &format!("response.output.{i}"), restored);
                }
                preceding.extend(events);
            }
            if self.unfinished_dispatcher() {
                return self.fail(Error::new(
                    "incomplete apply_patch namespace dispatcher received from upstream",
                ));
            }
            // Unproven calls stay ordinary; the bridge resolves their events
            // or passes them on.
            let (mut bytes, mut count) = (0, 0);
            for call in &mut self.records {
                if call.namespace.is_empty() && !call.ordinary {
                    bytes += held_size(&call.events);
                    count += call.events.len();
                    preceding.append(&mut call.events);
                }
            }
            self.release(bytes, count);
            if changed {
                event = to_bytes(&updated);
            }
        }
        match self.expand_dispatcher(event, &original) {
            Ok(events) => preceding.extend(events),
            Err(error) => return self.fail(error),
        }
        let mut out = Vec::new();
        for event in preceding {
            let (converted, error) = self.bridge.transform(&event);
            out.extend(converted);
            if error.is_some() {
                self.failed = true;
                self.clear();
                return (out, error);
            }
        }
        if terminal || kind == "response.failed" {
            self.closed = true;
            self.clear();
        }
        (out, None)
    }

    /// `Finish`: an error unless a JSON terminal event ended the response,
    /// and if the bridge failed or a patch call's arguments never completed.
    /// [`Bridge::finish`] checks only the arguments, since it also runs on a
    /// non-streaming response.
    pub fn finish(&self) -> Result<(), Error> {
        self.bridge.finish()?;
        self.check_overflow()?;
        if self.closed || !self.active {
            return Ok(());
        }
        if self.unfinished_dispatcher() {
            return Err(Error::new(
                "incomplete apply_patch namespace dispatcher received from upstream",
            ));
        }
        Err(Error::new(
            "incomplete apply_patch source response received from upstream",
        ))
    }

    /// `Stream`: converts one line of an SSE stream, without its line ending.
    /// A `data:` line that needs no change, and its `event:` line, keep their
    /// bytes. Otherwise each event becomes a complete frame, a `data:` line
    /// ending in a blank line, after an `event:` line naming its type if the
    /// source had one. `[DONE]` before the response ended fails the stream
    /// instead.
    pub fn stream(&mut self, line: &[u8]) -> (Vec<Vec<u8>>, Option<Error>) {
        if !self.active {
            return (vec![line.to_vec()], None);
        }
        if self.failed || self.transport_done {
            return (Vec::new(), None);
        }
        if line.starts_with(b"event:") {
            self.event_line = Some(line.to_vec());
            return (Vec::new(), None);
        }
        let Some(data) = line.strip_prefix(b"data:") else {
            return (vec![line.to_vec()], None);
        };
        let payload = go::trim_space(data);
        let (events, error) = if payload == b"[DONE]" {
            match self.finish() {
                Err(error) => self.fail(error),
                Ok(()) => {
                    // The JSON terminal event and the end of the transport
                    // are separate.
                    self.transport_done = true;
                    self.clear();
                    self.event_line = None;
                    return (vec![line.to_vec()], None);
                }
            }
        } else {
            self.transform(payload)
        };
        if error.is_none() && events.len() == 1 && events[0] == payload {
            let mut out: Vec<Vec<u8>> = self.event_line.take().into_iter().collect();
            out.push(line.to_vec());
            return (out, None);
        }
        let mut out = Vec::new();
        for event in events {
            if let Some(event_line) = &self.event_line {
                if event == payload {
                    out.push(event_line.clone());
                } else {
                    let kind = str_at(&parse(&event), "type");
                    out.push(format!("event: {kind}").into_bytes());
                }
            }
            out.push(frame(&event));
        }
        self.event_line = None;
        (out, error)
    }

    /// `FinishStream`: at the end of the stream, the one failure frame if
    /// the response never ended.
    pub fn finish_stream(&mut self) -> (Vec<Vec<u8>>, Option<Error>) {
        if self.failed || self.transport_done {
            return (Vec::new(), None);
        }
        match self.finish() {
            Ok(()) => (Vec::new(), None),
            Err(error) => {
                let (events, error) = self.fail(error);
                (events.iter().map(|event| frame(event)).collect(), error)
            }
        }
    }
}

/// `dispatcherKeys`: the keys a call's events may share.
fn dispatcher_keys(root: &Value) -> Vec<String> {
    let mut keys = Vec::new();
    for path in ["item.id", "item_id"] {
        let id = str_at(root, path);
        if !id.is_empty() {
            keys.push(format!("item:{id}"));
        }
    }
    for path in ["item.call_id", "call_id"] {
        let id = str_at(root, path);
        if !id.is_empty() {
            keys.push(format!("call:{id}"));
        }
    }
    if let Some(index) = get(root, "output_index") {
        keys.push(format!("index:{}", int_of(Some(index))));
    }
    keys
}

/// `dispatcherEventName`: the function an event names.
fn dispatcher_event_name(root: &Value) -> String {
    if get(root, "item").is_some() {
        str_at(root, "item.name")
    } else {
        str_at(root, "name")
    }
}

/// `patchDispatcherArguments`: a wrapper's `arguments`, a string's text or
/// any other value as written.
fn dispatcher_arguments(arguments: Option<LenientValue<'_>>) -> String {
    arguments.map_or_else(String::new, |arguments| arguments.text().to_owned())
}

/// gjson `Parse(text).Get(key).String()`, or `""` without `text`.
fn field(text: Option<&str>, key: &str) -> String {
    text.and_then(|text| parsed_get(text, key))
        .map_or_else(String::new, |value| value.string)
}

/// gjson `Parse(text).Get(key)`: only an object has fields.
fn parsed_get<'t>(text: &'t str, key: &str) -> Option<LenientValue<'t>> {
    let start = text.bytes().position(|byte| byte > b' ')?;
    if text.as_bytes()[start] != b'{' {
        return None;
    }
    lenient_get(&text[start..], key)
}

/// gjson `Array()`: an array's items, none for a missing value or `null`,
/// and any other value as the one item.
fn items(value: Option<&Value>) -> Vec<&Value> {
    match value {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.iter().collect(),
        Some(other) => vec![other],
    }
}

/// An event, read as `null` if it isn't JSON.
fn parse(event: &[u8]) -> Value {
    serde_json::from_slice(event).unwrap_or(Value::Null)
}

fn to_bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("a JSON value always serializes")
}

/// sjson `Set`, unless the value is already there. Returns whether the
/// value changed.
fn set(value: &mut Value, path: &str, new: Value) -> bool {
    if get(value, path) == Some(&new) {
        return false;
    }
    crate::json::set(value, path, new)
}

/// An event as one SSE frame.
fn frame(event: &[u8]) -> Vec<u8> {
    [&b"data: "[..], event, b"\n\n"].concat()
}

#[cfg(test)]
mod tests;
