// Ported from CLIProxyAPI internal/translator/common/apply_patch_responses.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The `apply_patch` bridge for a provider that speaks the Responses API but
//! only knows function tools.
//!
//! Codex declares `apply_patch` as a custom tool. [`normalize_request`]
//! declares it to the provider as a function instead, and turns earlier
//! custom calls in the history into function calls. A [`Bridge`] turns the
//! provider's function call events back into the custom tool call events the
//! client expects: `response.custom_tool_call_input.delta` and `.done`, and
//! output items of type `custom_tool_call` whose `input` is the patch text.
//! Other events pass through.
//!
//! An executor turns the bridge on by building a [`Bridge`] from the client's
//! request and passing each event through it. Upstream's Codex executor never
//! does; its xAI and Meta executors do.
//!
//! The bridge is strict. A call is converted only once its item ID, call ID
//! and output index are all known, so its events wait until then. Any
//! contradiction about a patch call, such as a changed ID, name or type,
//! arguments that aren't one input string, or a snapshot that disagrees with
//! what streamed, ends the stream with one `response.failed` event, and
//! [`Bridge::tool_input_error`] says why.
//!
//! Deviations from upstream:
//! - An event the bridge changes, including one it only renumbers, is written
//!   as compact JSON. Upstream edits the event's original text. Events it
//!   doesn't change keep their bytes.
//! - A payload that isn't JSON passes through unchanged. gjson reads what it
//!   can from it, and upstream renumbers it once a call has been converted.
//!   A payload that is valid JSON but that serde_json can't read, such as one
//!   nested too deeply, fails the stream, since it can't be checked.
//! - An `input`, an `output` or a tool choice's `tools` that isn't an array
//!   is read as empty. gjson reads any other value as a one-item array.
//! - [`normalize_request`] takes parsed JSON, so upstream's check that the
//!   request is valid JSON falls to the caller.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::mem;

use serde_json::{Value, json};

use super::input::{CallState, InputDecoder, InputError, failure};
use super::{description, is_custom_tool, parameters, wrap_input};
use crate::json::lenient::{self, Found};
use crate::json::{delete_path, go_value, int_of, path, raw, set_path, str_of};
use crate::responses_tools::{
    ToolDescriptor, collect_tool_descriptors, collect_tool_winners, qualify_namespace_tool_name,
};

/// Why the bridge failed a stream, or rejected a request. The client sees
/// only the `response.failed` event; this is for the executor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(Cow<'static, str>);

impl Error {
    /// An error with this message.
    pub fn new(message: &'static str) -> Self {
        Self(Cow::Borrowed(message))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn input_error(error: InputError) -> Error {
    Error(Cow::Owned(error.to_string()))
}

/// A tool the request declares: the winning declaration of its name, as
/// upstream's `ResponsesToolDescriptor`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tool {
    /// The name it is called by. A namespace's child is `<namespace>__<name>`.
    pub name: String,
    /// The name without its namespace, trimmed.
    pub local_name: String,
    /// The namespace's trimmed name, or empty for a tool declared directly.
    pub namespace: String,
    /// Whether it is the custom `apply_patch` tool.
    pub apply_patch: bool,
}

/// `QualifyResponsesNamespaceToolName`: the name a namespace's child is
/// called by. A name already qualified, or starting with `mcp__`, is kept.
pub fn qualify_tool_name(namespace: &str, name: &str) -> String {
    qualify_namespace_tool_name(namespace, name)
}

/// A value gjson finds at a key of text that need not be valid JSON, such
/// as the arguments of a call to a namespace's dispatcher function,
/// `{"name":…,"arguments":…}`, which name the child tool and hold its
/// arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LenientValue<'t> {
    /// gjson `String()`: a string decoded, any other value as text.
    pub string: String,
    /// The value as written, gjson's `Raw`, unless it is a string.
    pub raw: Option<&'t str>,
}

impl LenientValue<'_> {
    /// A string's text, or any other value as written.
    pub fn text(&self) -> &str {
        self.raw.unwrap_or(&self.string)
    }
}

/// gjson `Get(text, key)` for a plain key, on text that need not be valid
/// JSON: what gjson reads from it, or `None` if it finds nothing.
pub fn lenient_get<'t>(text: &'t str, key: &str) -> Option<LenientValue<'t>> {
    let found = lenient::get(text, key)?;
    let raw = match &found {
        Found::String(_) => None,
        Found::Number(raw) | Found::Literal(raw) | Found::Json(raw) => Some(*raw),
    };
    Some(LenientValue {
        string: found.into_string(),
        raw,
    })
}

/// `NormalizeApplyPatchResponsesRequest`: declares the custom `apply_patch`
/// tool as a function, and turns earlier custom `apply_patch` calls and their
/// outputs into function calls and outputs.
///
/// Which declaration wins a name is decided before anything changes. Other
/// declarations of a name that a custom `apply_patch` tool declares are
/// dropped, and a tool choice of the custom tool chooses the function.
/// Returns an error, leaving the request unchanged, when an earlier call's
/// `input` isn't a string.
pub fn normalize_request(request: &mut Value) -> Result<(), Error> {
    let changes = RequestChanges::new(request)?;
    changes.apply(request);
    Ok(())
}

/// What [`normalize_request`] changes, worked out before changing anything.
struct RequestChanges {
    tools: Option<Vec<Value>>,
    input: Vec<(usize, InputChange)>,
    tool_choice: Option<Value>,
}

enum InputChange {
    /// An `additional_tools` item's new `tools`.
    Tools(Vec<Value>),
    /// A custom `apply_patch` call, as a function call with these arguments.
    Call(String),
    /// The output of such a call.
    Output,
}

impl RequestChanges {
    fn new(request: &Value) -> Result<Self, Error> {
        let declarations = Declarations::new(request);
        let tools = match request.get("tools") {
            Some(Value::Array(tools)) => Some(declarations.tools(tools, "")),
            _ => None,
        };
        let mut input = Vec::new();
        if let Some(Value::Array(items)) = request.get("input") {
            let history: HashSet<Cow<'_, str>> = items
                .iter()
                .filter(|item| is_patch_call(item))
                .map(|item| str_of(item.get("call_id")))
                .collect();
            for (index, item) in items.iter().enumerate() {
                match &*str_of(item.get("type")) {
                    "additional_tools" => {
                        if let Some(Value::Array(tools)) = item.get("tools") {
                            input.push((index, InputChange::Tools(declarations.tools(tools, ""))));
                        }
                    }
                    "custom_tool_call" if is_patch_call(item) => {
                        let Some(Value::String(patch)) = item.get("input") else {
                            return Err(Error::new("apply_patch history input must be a string"));
                        };
                        input.push((index, InputChange::Call(wrap_input(patch))));
                    }
                    "custom_tool_call_output" if history.contains(&str_of(item.get("call_id"))) => {
                        input.push((index, InputChange::Output));
                    }
                    _ => {}
                }
            }
        }
        let tool_choice = match request.get("tool_choice") {
            Some(choice @ Value::Object(_)) => Some(declarations.choice(choice)),
            _ => None,
        };
        Ok(Self {
            tools,
            input,
            tool_choice,
        })
    }

    fn apply(self, request: &mut Value) {
        if let Some(tools) = self.tools {
            set_path(request, "tools", Value::Array(tools));
        }
        if let Some(Value::Array(items)) = request.get_mut("input") {
            for (index, change) in self.input {
                let Some(item) = items.get_mut(index) else {
                    continue;
                };
                match change {
                    InputChange::Tools(tools) => {
                        set_path(item, "tools", Value::Array(tools));
                    }
                    InputChange::Call(arguments) => {
                        set_path(item, "type", "function_call".into());
                        set_path(item, "arguments", arguments.into());
                        delete_path(item, "input");
                    }
                    InputChange::Output => {
                        set_path(item, "type", "function_call_output".into());
                    }
                }
            }
        }
        if let Some(choice) = self.tool_choice {
            set_path(request, "tool_choice", choice);
        }
    }
}

/// Whether a history item is a custom `apply_patch` call.
fn is_patch_call(item: &Value) -> bool {
    str_of(item.get("type")) == "custom_tool_call"
        && str_of(item.get("name")).trim() == "apply_patch"
}

/// The request's tool declarations, as they were before normalizing.
struct Declarations<'v> {
    winners: HashMap<String, ToolDescriptor<'v>>,
    /// The names some custom `apply_patch` tool declares, winning or not.
    affected: HashSet<String>,
}

impl<'v> Declarations<'v> {
    fn new(request: &'v Value) -> Self {
        Self {
            winners: collect_tool_winners(request),
            affected: collect_tool_descriptors(request)
                .into_iter()
                .filter(|(_, descriptor)| is_custom_tool(descriptor.tool))
                .map(|(name, _)| name)
                .collect(),
        }
    }

    /// `normalizeTools`: one list of declarations, with the losing
    /// declarations of an affected name dropped and the custom tool declared
    /// as a function. `tools` must be part of the request the winners came
    /// from: a declaration is compared to its winner by address.
    fn tools(&self, tools: &[Value], namespace: &str) -> Vec<Value> {
        let mut items = Vec::new();
        for tool in tools {
            let mut item = tool.clone();
            if str_of(tool.get("type")) == "namespace" {
                for key in ["tools", "children"] {
                    if let Some(Value::Array(children)) = tool.get(key) {
                        let children = self.tools(children, &str_of(tool.get("name")));
                        set_path(&mut item, key, Value::Array(children));
                        break;
                    }
                }
            } else {
                let mut name = str_of(tool.get("name"));
                if name.is_empty() {
                    name = str_of(path(tool, "function.name"));
                }
                let qualified = qualify_namespace_tool_name(namespace, &name);
                if let Some(winner) = self.winners.get(&qualified)
                    && self.affected.contains(&qualified)
                {
                    if !std::ptr::eq(winner.tool, tool) {
                        continue;
                    }
                    if is_custom_tool(tool) {
                        set_path(&mut item, "type", "function".into());
                        set_path(&mut item, "description", description(tool).into());
                        set_path(&mut item, "parameters", parameters());
                        delete_path(&mut item, "format");
                    }
                }
            }
            items.push(item);
        }
        items
    }

    /// `normalizeChoice`: a tool choice of the custom tool, here or among its
    /// `tools`, chooses the function instead.
    fn choice(&self, choice: &Value) -> Value {
        let mut out = choice.clone();
        let name = str_of(choice.get("name"));
        let namespace = str_of(choice.get("namespace"));
        let chosen = self
            .winners
            .get(&qualify_namespace_tool_name(&namespace, &name));
        if chosen.is_some_and(|winner| is_custom_tool(winner.tool))
            && str_of(choice.get("type")) == "custom"
        {
            set_path(&mut out, "type", "function".into());
        }
        if let Some(Value::Array(children)) = choice.get("tools") {
            let children = children.iter().map(|child| self.choice(child)).collect();
            set_path(&mut out, "tools", Value::Array(children));
        }
        out
    }
}

/// One event, with the text it came as while it is unchanged.
#[derive(Clone, Debug)]
struct Event {
    value: Value,
    text: Option<Vec<u8>>,
}

impl Event {
    fn parse(text: &[u8]) -> Option<Self> {
        let value = serde_json::from_slice(text).ok()?;
        Some(Self {
            value,
            text: Some(text.to_vec()),
        })
    }

    fn new(value: Value) -> Self {
        Self { value, text: None }
    }

    /// The value, to change: the event is written anew.
    fn value_mut(&mut self) -> &mut Value {
        self.text = None;
        &mut self.value
    }

    fn into_bytes(self) -> Vec<u8> {
        self.text.unwrap_or_else(|| {
            serde_json::to_vec(&self.value).expect("a JSON value always serializes")
        })
    }
}

fn into_bytes(events: Vec<Event>) -> Vec<Vec<u8>> {
    events.into_iter().map(Event::into_bytes).collect()
}

/// `responsesPatchRecord`: what is known of one output item that may be a
/// patch call, from every identity it has been seen with.
struct Record {
    state: CallState,
    /// The call's name and namespace, as the client knows them.
    name: String,
    namespace: String,
    /// The item's type.
    kind: String,
    /// The tool's qualified name.
    qualified: String,
    /// The arguments streamed so far.
    source: String,
    /// Whether it calls the custom `apply_patch` tool.
    patch: bool,
    named: bool,
    added: bool,
    input_done: bool,
    item_done: bool,
    /// The last complete arguments seen, checked.
    snapshot: Option<String>,
    /// The output item, as last sent.
    completed_item: Value,
    /// Events waiting until the call's identity is known.
    pending: Vec<Event>,
    /// A contradiction, kept until the item turns out to be a patch call.
    evidence: Option<Error>,
}

impl Record {
    fn new() -> Self {
        Self {
            state: CallState::new(String::new(), String::new(), -1),
            name: String::new(),
            namespace: String::new(),
            kind: String::new(),
            qualified: String::new(),
            source: String::new(),
            patch: false,
            named: false,
            added: false,
            input_done: false,
            item_done: false,
            snapshot: None,
            completed_item: Value::Null,
            pending: Vec::new(),
            evidence: None,
        }
    }

    fn identity_ready(&self) -> bool {
        !self.state.item_id.is_empty()
            && !self.state.call_id.is_empty()
            && self.state.output_index >= 0
    }

    /// `snapshot`: checks complete arguments from an event and keeps them.
    /// Partial ones are ignored, unless `last` says they're final.
    fn snapshot(&mut self, arguments: Option<&Value>, last: bool) -> Result<(), Error> {
        let Some(arguments) = arguments else {
            return Ok(());
        };
        let Value::String(arguments) = arguments else {
            return Err(Error::new(
                "apply_patch arguments snapshot must be a string",
            ));
        };
        if arguments.is_empty() && !last {
            return Ok(());
        }
        let mut decoder = InputDecoder::default();
        decoder.finish(arguments).map_err(input_error)?;
        if let Some(previous) = &self.snapshot
            && decoded(previous) != decoder.input()
        {
            return Err(Error::new("conflicting apply_patch arguments snapshot"));
        }
        if !decoder.input().starts_with(self.state.input()) {
            return Err(Error::new(
                "apply_patch snapshot conflicts with streamed input",
            ));
        }
        self.snapshot = Some(arguments.clone());
        Ok(())
    }
}

/// The input that checked arguments hold.
fn decoded(arguments: &str) -> String {
    let mut decoder = InputDecoder::default();
    // Only arguments that finished cleanly are kept as a snapshot.
    let _ = decoder.finish(arguments);
    decoder.input().to_owned()
}

/// A stand-in for an item an event doesn't have.
static MISSING: Value = Value::Null;

/// `ApplyPatchResponsesBridge`: converts one response's events, without SSE
/// framing. It belongs to the executor serving the response.
pub struct Bridge {
    tools: HashMap<String, Tool>,
    records: Vec<Record>,
    by_item_id: HashMap<String, usize>,
    by_call_id: HashMap<String, usize>,
    by_output_index: HashMap<i64, usize>,
    sequence: i64,
    last_sequence: i64,
    response_id: String,
    failed: bool,
    terminal: bool,
    active: bool,
    converted: bool,
    error: Option<Error>,
}

impl Bridge {
    /// `NewApplyPatchResponsesBridge`: a bridge for a response to
    /// `original_request`, the client's request before [`normalize_request`].
    /// It is active if the request declares the custom `apply_patch` tool.
    pub fn new(original_request: &Value) -> Self {
        let tools: HashMap<String, Tool> = collect_tool_winners(original_request)
            .into_iter()
            .map(|(name, descriptor)| {
                let tool = Tool {
                    name: name.clone(),
                    local_name: descriptor.local_name,
                    namespace: descriptor.namespace,
                    apply_patch: is_custom_tool(descriptor.tool),
                };
                (name, tool)
            })
            .collect();
        let active = tools.values().any(|tool| tool.apply_patch);
        Self {
            tools,
            records: Vec::new(),
            by_item_id: HashMap::new(),
            by_call_id: HashMap::new(),
            by_output_index: HashMap::new(),
            sequence: 0,
            last_sequence: 0,
            response_id: String::new(),
            failed: false,
            terminal: false,
            active,
            converted: false,
            error: None,
        }
    }

    /// Whether the request declares the custom `apply_patch` tool. An inactive
    /// bridge passes everything through.
    pub fn active(&self) -> bool {
        self.active
    }

    /// The winning declaration of each tool name in the request.
    pub fn tools(&self) -> impl Iterator<Item = &Tool> {
        self.tools.values()
    }

    /// The winning declaration of the tool `name`, qualified by `namespace`.
    pub fn tool(&self, namespace: &str, name: &str) -> Option<&Tool> {
        self.tools
            .get(&qualify_namespace_tool_name(namespace, name))
    }

    /// `ToolInputError`: why the bridge failed, if it has.
    pub fn tool_input_error(&self) -> Option<&Error> {
        self.error.as_ref()
    }

    /// `next`. Upstream's `int` is 64 bits and wraps, and `sequence` can start
    /// from a number upstream supplied, so this wraps too.
    fn next(&mut self) -> i64 {
        self.sequence = self.sequence.wrapping_add(1);
        self.sequence
    }

    /// `failure`: the one `response.failed` event. Nothing once the response
    /// has failed or ended.
    fn failure(&mut self, error: Error) -> (Vec<Event>, Option<Error>) {
        if self.failed || self.terminal {
            return (Vec::new(), None);
        }
        self.failed = true;
        self.error = Some(error.clone());
        let sequence = self.next();
        let event = Event::new(failure(&self.response_id, sequence));
        (vec![event], Some(error))
    }

    /// `Fail`: ends the response for a reason the executor found, as the
    /// bridge ends it for its own: one `response.failed` event, then nothing.
    /// Returns no events and no error if it has already failed or ended.
    pub fn fail(&mut self, error: Error) -> (Vec<Vec<u8>>, Option<Error>) {
        let (events, error) = self.failure(error);
        (into_bytes(events), error)
    }

    /// The tool an item calls, by its name and namespace.
    fn descriptor(&self, item: &Value) -> Option<Tool> {
        let name =
            qualify_namespace_tool_name(&str_of(item.get("namespace")), &str_of(item.get("name")));
        self.tools.get(&name).cloned()
    }

    /// Whether the tool a record calls is a namespace's child.
    fn namespaced(&self, record: usize) -> bool {
        self.tools
            .get(&self.records[record].qualified)
            .is_some_and(|tool| !tool.namespace.is_empty())
    }

    /// `resolve`: the record an event's item belongs to, by every identity
    /// the event gives: item ID, call ID and output index. Identities that
    /// contradict each other are kept as evidence, and are an error once the
    /// item is known to be a patch call.
    fn resolve(&mut self, event: &Value, item: &Value) -> Result<usize, Error> {
        let ids = [str_of(event.get("item_id")), str_of(item.get("id"))];
        let calls = [str_of(event.get("call_id")), str_of(item.get("call_id"))];
        let index = event.get("output_index").map(int_of);
        let mut matched = Vec::new();
        let mut add_match = |record: Option<&usize>| {
            if let Some(&record) = record
                && !matched.contains(&record)
            {
                matched.push(record);
            }
        };
        for id in ids.iter().filter(|id| !id.is_empty()) {
            add_match(self.by_item_id.get(&**id));
        }
        for id in calls.iter().filter(|id| !id.is_empty()) {
            add_match(self.by_call_id.get(&**id));
        }
        if let Some(index) = index {
            add_match(self.by_output_index.get(&index));
        }
        // Records are numbered in the order they were made.
        let r = match matched.iter().min() {
            Some(&r) => r,
            None => {
                self.records.push(Record::new());
                self.records.len() - 1
            }
        };
        let record = &self.records[r];
        let mut bad = matched.len() > 1;
        for id in ids.iter().filter(|id| !id.is_empty()) {
            if !record.state.item_id.is_empty() && record.state.item_id != *id {
                bad = true;
            }
        }
        for id in calls.iter().filter(|id| !id.is_empty()) {
            if !record.state.call_id.is_empty() && record.state.call_id != *id {
                bad = true;
            }
        }
        if !ids[0].is_empty() && !ids[1].is_empty() && ids[0] != ids[1] {
            bad = true;
        }
        if !calls[0].is_empty() && !calls[1].is_empty() && calls[0] != calls[1] {
            bad = true;
        }
        if let Some(index) = index
            && record.state.output_index >= 0
            && record.state.output_index != index
        {
            bad = true;
        }
        let descriptor = self.descriptor(item);
        let mut incoming_patch = descriptor.as_ref().is_some_and(|tool| tool.apply_patch)
            && str_of(item.get("type")) != "custom_tool_call";
        if bad {
            let error = Error::new("conflicting apply_patch call identity");
            self.records[r].evidence = Some(error.clone());
            for &candidate in &matched {
                self.records[candidate].evidence = Some(error.clone());
                incoming_patch |= self.records[candidate].patch;
            }
            // Unmatched identities point here too, so that a later event
            // can't start a fresh record and lose the contradiction.
            for id in ids.iter().filter(|id| !id.is_empty()) {
                self.by_item_id.entry(id.to_string()).or_insert(r);
            }
            for id in calls.iter().filter(|id| !id.is_empty()) {
                self.by_call_id.entry(id.to_string()).or_insert(r);
            }
            if let Some(index) = index {
                self.by_output_index.entry(index).or_insert(r);
            }
            if self.records[r].patch || incoming_patch {
                return Err(error);
            }
        } else {
            for id in ids.iter().filter(|id| !id.is_empty()) {
                self.records[r].state.item_id = id.to_string();
                self.by_item_id.insert(id.to_string(), r);
            }
            for id in calls.iter().filter(|id| !id.is_empty()) {
                self.records[r].state.call_id = id.to_string();
                self.by_call_id.insert(id.to_string(), r);
            }
            if let Some(index) = index {
                self.records[r].state.output_index = index;
                self.by_output_index.insert(index, r);
            }
        }
        let record = &mut self.records[r];
        let kind = str_of(item.get("type"));
        if !kind.is_empty() {
            if !record.kind.is_empty() && record.kind != kind {
                record.evidence = Some(Error::new("conflicting apply_patch call type"));
            }
            if record.kind.is_empty() {
                record.kind = kind.into_owned();
            }
        }
        let name = str_of(item.get("name"));
        if !name.is_empty() {
            let namespace = str_of(item.get("namespace"));
            let qualified = match &descriptor {
                Some(tool) => tool.name.clone(),
                None => qualify_namespace_tool_name(&namespace, &name),
            };
            if record.named && record.qualified != qualified {
                record.evidence = Some(Error::new("conflicting apply_patch call name"));
            }
            record.named = true;
            record.qualified = qualified;
            match descriptor {
                Some(tool) => {
                    record.name = tool.local_name;
                    record.namespace = tool.namespace;
                }
                None => {
                    record.name = name.into_owned();
                    record.namespace = namespace.into_owned();
                }
            }
        }
        if incoming_patch {
            record.patch = true;
        }
        if record.patch
            && let Some(evidence) = &record.evidence
        {
            return Err(evidence.clone());
        }
        Ok(r)
    }

    /// `CheckIdentity`: records the identities an event gives, before the
    /// executor knows which tool a namespace dispatcher's call is for. The
    /// item's name and namespace are left out: they name the dispatcher, not
    /// the tool it calls.
    pub fn check_identity(&mut self, event: &[u8]) -> Result<(), Error> {
        let root: Value = serde_json::from_slice(event).unwrap_or(Value::Null);
        let item = root.get("item").map(|item| {
            let mut item = item.clone();
            delete_path(&mut item, "name");
            delete_path(&mut item, "namespace");
            item
        });
        self.resolve(&root, item.as_ref().unwrap_or(&MISSING))
            .map(|_| ())
    }

    /// `restoreItem`: an output item as the client expects it: a patch call
    /// as a custom tool call with `input`, and a namespace's child under its
    /// own name and namespace.
    fn restore_item(&self, mut item: Value, r: usize, input: &str, added: bool) -> Value {
        let record = &self.records[r];
        if record.patch {
            set_path(&mut item, "type", "custom_tool_call".into());
            delete_path(&mut item, "arguments");
            set_path(&mut item, "input", input.into());
        }
        if let Some(tool) = self.tools.get(&record.qualified)
            && !tool.namespace.is_empty()
        {
            set_path(&mut item, "name", tool.local_name.as_str().into());
            set_path(&mut item, "namespace", tool.namespace.as_str().into());
        }
        if record.patch && !added {
            if !record.state.item_id.is_empty() {
                set_path(&mut item, "id", record.state.item_id.as_str().into());
            }
            if !record.state.call_id.is_empty() {
                set_path(&mut item, "call_id", record.state.call_id.as_str().into());
            }
            set_path(&mut item, "name", record.name.as_str().into());
        }
        item
    }

    /// `itemEvent`: an `output_item` event for a record.
    fn item_event(&mut self, kind: &str, item: Value, r: usize) -> Event {
        let output_index = self.records[r].state.output_index;
        let sequence = self.next();
        Event::new(json!({
            "type": kind,
            "output_index": output_index,
            "sequence_number": sequence,
            "item": item,
        }))
    }

    /// `patchEvent`: converts one event of a patch call whose identity is
    /// known.
    fn patch_event(&mut self, event: &Value, r: usize) -> Result<Vec<Event>, Error> {
        if !self.records[r].identity_ready() {
            return Err(Error::new("unresolved apply_patch call identity"));
        }
        let kind = str_of(event.get("type"));
        let item = event.get("item");
        self.converted = true;
        let mut out = Vec::new();
        if let Some(item) = item {
            if str_of(item.get("type")) != "function_call" {
                return Err(Error::new("conflicting apply_patch call type"));
            }
            self.records[r].snapshot(item.get("arguments"), kind == "response.output_item.done")?;
        }
        if !self.records[r].added {
            let record = &self.records[r];
            let mut added = match item {
                Some(item) => item.clone(),
                None => json!({"type": "function_call", "name": "", "arguments": ""}),
            };
            set_path(&mut added, "name", record.name.as_str().into());
            if !record.state.item_id.is_empty() {
                set_path(&mut added, "id", record.state.item_id.as_str().into());
            }
            if !record.state.call_id.is_empty() {
                set_path(&mut added, "call_id", record.state.call_id.as_str().into());
            }
            if !record.namespace.is_empty() {
                set_path(&mut added, "namespace", record.namespace.as_str().into());
            }
            let added = self.restore_item(added, r, "", true);
            out.push(self.item_event("response.output_item.added", added, r));
            self.records[r].added = true;
        }
        match &*kind {
            "response.function_call_arguments.delta" => {
                let fragment = str_of(event.get("delta"));
                let record = &mut self.records[r];
                if record.input_done {
                    if !fragment.is_empty() {
                        return Err(Error::new(
                            "apply_patch arguments received after completion",
                        ));
                    }
                    return Ok(out);
                }
                record.source.push_str(&fragment);
                let delta = record
                    .state
                    .push_arguments(fragment.as_bytes())
                    .map_err(input_error)?;
                if let Some(snapshot) = &record.snapshot
                    && !decoded(snapshot).starts_with(record.state.input())
                {
                    return Err(Error::new("apply_patch stream conflicts with snapshot"));
                }
                if !delta.is_empty() {
                    let sequence = self.next();
                    out.push(Event::new(
                        self.records[r].state.input_delta(&delta, sequence),
                    ));
                }
            }
            "response.function_call_arguments.done" | "response.output_item.done" => {
                let arguments = match item {
                    Some(item) => item.get("arguments"),
                    None => event.get("arguments"),
                };
                if arguments.is_some() {
                    self.records[r].snapshot(arguments, true)?;
                }
                let record = &mut self.records[r];
                let last = record.snapshot.as_ref().unwrap_or(&record.source).clone();
                let (tail, input) = record
                    .state
                    .finish_arguments(last.as_bytes())
                    .map_err(input_error)?;
                if !record.input_done {
                    if !tail.is_empty() && !record.source.is_empty() {
                        let sequence = self.next();
                        out.push(Event::new(
                            self.records[r].state.input_delta(&tail, sequence),
                        ));
                    }
                    let sequence = self.next();
                    out.push(Event::new(
                        self.records[r].state.input_done(&input, sequence),
                    ));
                    self.records[r].input_done = true;
                }
                if kind == "response.output_item.done" && !self.records[r].item_done {
                    let item = item.cloned().unwrap_or(Value::Null);
                    let completed = self.restore_item(item, r, &input, false);
                    self.records[r].completed_item = completed.clone();
                    out.push(self.item_event(&kind, completed, r));
                    self.records[r].item_done = true;
                }
            }
            _ => {}
        }
        Ok(out)
    }

    /// `transformItemEvent`: an `output_item` or `function_call_arguments`
    /// event. A patch call's events wait until its identity is known, and an
    /// unnamed function call's until its name is.
    fn transform_item_event(&mut self, mut event: Event) -> Result<Vec<Event>, Error> {
        let has_item = event.value.get("item").is_some();
        // Arguments events can give a late name and identity at the top level.
        // Upstream copies each with gjson `Value()`, so a number is read as a
        // float64: one above 2^53 changes, and then contradicts the same
        // number at the root.
        let identity = match event.value.get("name") {
            Some(_) if !has_item => {
                let mut identity = json!({"type": "function_call"});
                for key in ["name", "namespace", "call_id"] {
                    if let Some(value) = event.value.get(key) {
                        set_path(&mut identity, key, go_value(value));
                    }
                }
                Some(identity)
            }
            _ => None,
        };
        let item = match &identity {
            Some(identity) => identity,
            None => event.value.get("item").unwrap_or(&MISSING),
        };
        let r = self.resolve(&event.value, item)?;
        // A known name is provenance, not readiness: the events wait until
        // both IDs and the output index can identify every event sent.
        let record = &self.records[r];
        let unnamed = !record.named && (record.kind.is_empty() || record.kind == "function_call");
        if unnamed || (record.patch && !record.identity_ready()) {
            if record.patch {
                let arguments = if has_item {
                    item.get("arguments")
                } else {
                    event.value.get("arguments")
                };
                let kind = str_of(event.value.get("type"));
                let last = kind == "response.output_item.done"
                    || kind == "response.function_call_arguments.done";
                self.records[r].snapshot(arguments, last)?;
            }
            self.records[r].pending.push(event);
            return Ok(Vec::new());
        }
        let pending = mem::take(&mut self.records[r].pending);
        if self.records[r].patch {
            let mut out = Vec::new();
            for source in pending.iter().chain([&event]) {
                out.extend(self.patch_event(&source.value, r)?);
            }
            return Ok(out);
        }
        let mut out = pending;
        if has_item && self.records[r].kind == "function_call" && self.namespaced(r) {
            let item = event.value.get("item").cloned().unwrap_or(Value::Null);
            let restored = self.restore_item(item, r, "", false);
            set_path(event.value_mut(), "item", restored);
        }
        if str_of(event.value.get("type")) == "response.output_item.done"
            && let Some(item) = event.value.get("item")
        {
            self.records[r].item_done = true;
            self.records[r].completed_item = item.clone();
        }
        out.push(event);
        Ok(out)
    }

    /// `envelope`: a response, or a final event holding one. Patch calls in
    /// its output are converted, and completed items it leaves out are put
    /// back. Returns it with the events to send before it.
    fn envelope(&mut self, mut event: Event, stream: bool) -> Result<(Event, Vec<Event>), Error> {
        let nested = event.value.get("response").is_some();
        let output_path = if nested { "response.output" } else { "output" };
        let source = match path(&event.value, output_path) {
            Some(Value::Array(items)) => items.clone(),
            _ => Vec::new(),
        };
        let mut preceding = Vec::new();
        let mut seen = HashSet::new();
        let mut items = Vec::with_capacity(source.len());
        let mut changed = false;
        for (i, item) in source.iter().enumerate() {
            let mut index = i64::try_from(i).unwrap_or(i64::MAX);
            // A final snapshot may leave out earlier completed items, so an
            // item's position isn't its identity.
            let id = str_of(item.get("id"));
            let call_id = str_of(item.get("call_id"));
            let known = self
                .by_item_id
                .get(&*id)
                .or_else(|| self.by_call_id.get(&*call_id))
                .copied();
            match known {
                Some(known) if self.records[known].state.output_index >= 0 => {
                    index = self.records[known].state.output_index;
                }
                None if !id.is_empty() || !call_id.is_empty() => {
                    let taken = self.by_output_index.get(&index).is_some_and(|&previous| {
                        let previous = &self.records[previous].state;
                        !previous.item_id.is_empty() || !previous.call_id.is_empty()
                    });
                    if taken {
                        for record in &self.records {
                            if record.state.output_index >= index {
                                // An output index is upstream's, and Go's wraps.
                                index = record.state.output_index.wrapping_add(1);
                            }
                        }
                    }
                }
                _ => {}
            }
            let done = json!({
                "type": "response.output_item.done",
                "output_index": index,
                "item": item,
            });
            let r = self.resolve(&done, item)?;
            seen.insert(r);
            if self.records[r].patch {
                for pending in mem::take(&mut self.records[r].pending) {
                    preceding.extend(self.patch_event(&pending.value, r)?);
                }
                preceding.extend(self.patch_event(&done, r)?);
                let input = self.records[r].state.input().to_owned();
                items.push(self.restore_item(item.clone(), r, &input, false));
                changed = true;
            } else if str_of(item.get("type")) == "function_call" && self.namespaced(r) {
                items.push(self.restore_item(item.clone(), r, "", false));
                changed = true;
            } else {
                items.push(item.clone());
            }
        }
        for r in 0..self.records.len() {
            if seen.contains(&r) || (!self.records[r].patch && !self.converted) {
                continue;
            }
            if self.records[r].input_done && !self.records[r].item_done {
                let input = self.records[r].state.input().to_owned();
                let item = json!({"type": "function_call", "status": "completed"});
                let completed = self.restore_item(item, r, &input, false);
                self.records[r].completed_item = completed.clone();
                preceding.push(self.item_event("response.output_item.done", completed, r));
                self.records[r].item_done = true;
            }
            if self.records[r].item_done {
                let index = usize::try_from(self.records[r].state.output_index)
                    .ok()
                    .filter(|&index| index <= items.len())
                    .unwrap_or(items.len());
                items.insert(index, self.records[r].completed_item.clone());
            }
        }
        if changed || items.len() != source.len() {
            set_path(event.value_mut(), output_path, Value::Array(items));
        }
        if stream {
            self.finish()?;
            for record in &mut self.records {
                preceding.append(&mut record.pending);
            }
            if self.converted {
                let sequence = self.next();
                set_path(event.value_mut(), "sequence_number", sequence.into());
            }
        }
        Ok((event, preceding))
    }

    /// `Transform`: converts one event, a JSON payload without SSE framing,
    /// into the events to send. A failure gives one `response.failed` event
    /// and the error, and after it, or after the response ends, nothing
    /// comes out.
    pub fn transform(&mut self, event: &[u8]) -> (Vec<Vec<u8>>, Option<Error>) {
        if self.failed || self.terminal {
            return (Vec::new(), None);
        }
        if !self.active {
            return (vec![event.to_vec()], None);
        }
        let Some(parsed) = Event::parse(event) else {
            if unreadable(event) {
                let (events, error) = self.failure(Error::new(UNREADABLE));
                return (into_bytes(events), error);
            }
            return (vec![event.to_vec()], None);
        };
        let (events, error) = self.transform_event(parsed);
        (into_bytes(events), error)
    }

    fn transform_event(&mut self, event: Event) -> (Vec<Event>, Option<Error>) {
        let id = str_of(path(&event.value, "response.id"));
        if !id.is_empty() {
            self.response_id = id.into_owned();
        }
        let sequence = event.value.get("sequence_number").map_or(0, int_of);
        if sequence > self.sequence {
            self.sequence = sequence;
        }
        let kind = str_of(event.value.get("type")).into_owned();
        // Native custom tool events are opaque. Only a stream that has a
        // converted call needs its other events renumbered.
        let native_custom = kind.starts_with("response.custom_tool_call_input.")
            || str_of(path(&event.value, "item.type")) == "custom_tool_call";
        let result = match kind.as_str() {
            "response.output_item.added"
            | "response.output_item.done"
            | "response.function_call_arguments.delta"
            | "response.function_call_arguments.done" => self.transform_item_event(event),
            "response.completed" | "response.incomplete" | "response.done" => {
                match self.envelope(event, true) {
                    Ok((last, mut out)) => {
                        out.push(last);
                        self.terminal = true;
                        Ok(out)
                    }
                    Err(error) => Err(error),
                }
            }
            "response.failed" => {
                self.terminal = true;
                Ok(vec![event])
            }
            _ => Ok(vec![event]),
        };
        let mut out = match result {
            Ok(out) => out,
            Err(error) => return self.failure(error),
        };
        for event in &mut out {
            let mut sequence = event.value.get("sequence_number").map_or(0, int_of);
            if self.converted && !native_custom && sequence <= self.last_sequence {
                sequence = self.next();
                set_path(event.value_mut(), "sequence_number", sequence.into());
            }
            if sequence > self.last_sequence {
                self.last_sequence = sequence;
            }
        }
        (out, None)
    }

    /// `TransformNonStream`: converts a whole response, or a final event
    /// holding one. On an error, the bridge has failed.
    pub fn transform_non_stream(&mut self, response: &[u8]) -> Result<Vec<u8>, Error> {
        if !self.active {
            return Ok(response.to_vec());
        }
        match Event::parse(response) {
            Some(event) => self.non_stream(event).map(Event::into_bytes),
            None if unreadable(response) => Err(self.non_stream_error(Error::new(UNREADABLE))),
            None => Ok(response.to_vec()),
        }
    }

    /// [`Bridge::transform_non_stream`] for a parsed response.
    pub(crate) fn transform_non_stream_value(&mut self, response: Value) -> Result<Value, Error> {
        if !self.active {
            return Ok(response);
        }
        self.non_stream(Event::new(response))
            .map(|event| event.value)
    }

    fn non_stream(&mut self, response: Event) -> Result<Event, Error> {
        match self.envelope(response, false) {
            Ok((response, _)) => Ok(response),
            Err(error) => Err(self.non_stream_error(error)),
        }
    }

    fn non_stream_error(&mut self, error: Error) -> Error {
        self.failed = true;
        self.error = Some(error.clone());
        error
    }

    /// `Finish`: an error if the bridge has failed, or if a patch call's
    /// arguments never completed and the response hasn't ended.
    pub fn finish(&self) -> Result<(), Error> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        if self.terminal {
            return Ok(());
        }
        if self
            .records
            .iter()
            .any(|record| record.patch && !record.input_done)
        {
            return Err(Error::new(
                "incomplete apply_patch tool arguments received from upstream",
            ));
        }
        Ok(())
    }
}

/// Not upstream's: the error for an event that is valid JSON but that
/// serde_json can't read.
const UNREADABLE: &str = "unreadable upstream event in apply_patch stream";

/// Whether a payload serde_json couldn't read is valid JSON nonetheless.
fn unreadable(payload: &[u8]) -> bool {
    raw::valid(String::from_utf8_lossy(payload).trim())
}

#[cfg(test)]
mod tests;
