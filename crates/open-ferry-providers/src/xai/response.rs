// Ported from CLIProxyAPI internal/runtime/executor/xai_executor_response.go
// (xaiInternalXSearchResponseFilter, xaiEffectiveDeclaredToolType,
// xaiIsInternalXSearchToolName, xaiResponseCallDeclaredType,
// xaiIsInternalXSearchCallID, xaiIsInternalXSearchCall,
// xaiNamespaceRestorer, unwrapXAIDispatcherArguments,
// restoreXAINamespaceToolCalls, restoreXAIClientWebSearchName,
// xaiPatchCompletedOutput) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! xAI's response events, turned back into what the client sent.
//!
//! - A flattened tool's call gets its own name and `namespace` back, and a
//!   folded namespace's dispatcher call becomes the child's call, with the
//!   child's arguments ([`NamespaceRestorer`]).
//! - A call of the renamed client `web_search` function gets its name back
//!   ([`restore_client_web_search_name`]).
//! - With Grok's X search tool sent, the calls it makes of its own
//!   subtools (`x_keyword_search` and the like), which xAI shows as client
//!   tool calls, are hidden along with every event about them, and the
//!   output indexes after them close up ([`XSearchFilter`]).
//! - A completed response that came without `output` gets the items of the
//!   `response.output_item.done` events before it
//!   ([`patch_completed_output`]; the items are kept as Codex's are).
//!
//! Each takes an event's JSON and gives it back as it was when it isn't
//! valid JSON or there's nothing to do.
//!
//! Deviations from upstream:
//! - A changed event is written again by `serde_json`, where sjson edits
//!   it in place; keys keep their order.
//! - A dispatcher call's child arguments that aren't a string are written
//!   compactly, where upstream copies their text.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::tools::{self, ClientToolKey, NamespaceRefs};
use crate::codex::terminal::OutputItems;
use crate::codex::usage::ensure_responses_usage_details;
use crate::json::{self, get, int_of, set, str_of};

/// The X search subtools xAI calls itself (`xaiIsInternalXSearchToolName`).
const X_SEARCH_SUBTOOLS: [&str; 4] = [
    "x_user_search",
    "x_semantic_search",
    "x_keyword_search",
    "x_thread_fetch",
];

/// How the IDs of X search's own calls start (`xaiIsInternalXSearchCallID`).
const X_SEARCH_CALL_ID_PREFIX: &str = "xs_call";

/// An event's `data` parsed, or `None` when it isn't valid JSON.
pub(super) fn parse(data: &[u8]) -> Option<Value> {
    serde_json::from_slice(data).ok()
}

/// An event written out, or its original `data` should that fail.
pub(super) fn write(value: &Value, data: Vec<u8>) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or(data)
}

/// gjson `String()` of `value`'s `key`, trimmed.
fn trimmed(value: &Value, key: &str) -> String {
    str_of(value.get(key)).trim().to_owned()
}

/// The type a response call of `item_type` was declared as
/// (`xaiResponseCallDeclaredType`): a client's custom tool is sent as a
/// function, so only a `function_call` can be the client's.
fn call_declared_type(item_type: &str) -> Option<&'static str> {
    match item_type.trim() {
        "function_call" => Some(tools::FUNCTION),
        "custom_tool_call" => Some(tools::CUSTOM),
        _ => None,
    }
}

/// Whether an output item is one of X search's own calls, to hide from the
/// client (`xaiIsInternalXSearchCall`): a call of an X search subtool with
/// no namespace, whose call ID starts `xs_call` or which the client didn't
/// declare as a tool of that type.
pub(crate) fn is_internal_x_search_call(
    item: Option<&Value>,
    client_tools: &HashSet<ClientToolKey>,
) -> bool {
    let Some(item) = item else {
        return false;
    };
    let Some(declared_type) = call_declared_type(&str_of(item.get("type"))) else {
        return false;
    };
    let name = trimmed(item, "name");
    if !X_SEARCH_SUBTOOLS.contains(&name.as_str()) {
        return false;
    }
    // A namespaced call is a restored client tool's.
    if !trimmed(item, "namespace").is_empty() {
        return false;
    }
    if trimmed(item, "call_id").starts_with(X_SEARCH_CALL_ID_PREFIX) {
        return true;
    }
    !client_tools.contains(&ClientToolKey {
        namespace: String::new(),
        name,
        tool_type: declared_type.to_owned(),
    })
}

/// Hides X search's own calls (`xaiInternalXSearchResponseFilter`): the
/// event announcing one is dropped, and so is each later event about it (by
/// output index, item ID or call ID); later output indexes close up, and a
/// completed response's `output` loses them.
#[derive(Debug, Default)]
pub(crate) struct XSearchFilter {
    /// Whether the request sent Grok's X search tool.
    enabled: bool,
    /// The tools the client declared.
    client_tools: HashSet<ClientToolKey>,
    dropped_indexes: HashSet<i64>,
    dropped_ids: HashSet<String>,
}

impl XSearchFilter {
    /// A filter for a request that sent X search (`enabled`) and declared
    /// `client_tools`.
    pub(crate) fn new(enabled: bool, client_tools: HashSet<ClientToolKey>) -> Self {
        Self {
            enabled,
            client_tools,
            ..Self::default()
        }
    }

    /// The event as the client gets it, or `None` when it is dropped.
    pub(crate) fn apply(&mut self, data: Vec<u8>) -> Option<Vec<u8>> {
        if !self.enabled || data.is_empty() {
            return Some(data);
        }
        let Some(mut event) = parse(&data) else {
            return Some(data);
        };
        if let Some(item) = event.get("item")
            && is_internal_x_search_call(Some(item), &self.client_tools)
        {
            if let Some(index) = event.get("output_index") {
                self.dropped_indexes.insert(int_of(Some(index)));
            }
            for key in ["id", "call_id"] {
                let id = trimmed(item, key);
                if !id.is_empty() {
                    self.dropped_ids.insert(id);
                }
            }
            return None;
        }
        let mut changed = self.filter_completed_output(&mut event);
        if self.references_dropped(&event) {
            return None;
        }
        changed |= self.compact_output_index(&mut event);
        Some(if changed { write(&event, data) } else { data })
    }

    /// `referencesDroppedItem`.
    fn references_dropped(&self, event: &Value) -> bool {
        if let Some(index) = event.get("output_index")
            && self.dropped_indexes.contains(&int_of(Some(index)))
        {
            return true;
        }
        ["item_id", "call_id"].into_iter().any(|key| {
            let id = trimmed(event, key);
            !id.is_empty() && self.dropped_ids.contains(&id)
        })
    }

    /// `compactOutputIndex`: the output index less the dropped ones before
    /// it.
    fn compact_output_index(&self, event: &mut Value) -> bool {
        let Some(index) = event.get("output_index") else {
            return false;
        };
        let original = int_of(Some(index));
        let removed = self
            .dropped_indexes
            .iter()
            .filter(|&&dropped| dropped < original)
            .count();
        let Ok(removed) = i64::try_from(removed) else {
            return false;
        };
        removed > 0 && set(event, "output_index", Value::from(original - removed))
    }

    /// `filterCompletedOutput`: `response.output` without X search's calls.
    fn filter_completed_output(&self, event: &mut Value) -> bool {
        let Some(Value::Array(output)) = json::get_mut(event, "response.output") else {
            return false;
        };
        let before = output.len();
        output.retain(|item| !is_internal_x_search_call(Some(item), &self.client_tools));
        output.len() != before
    }
}

/// Gives flattened and folded tools' calls back their own names and
/// namespaces (`xaiNamespaceRestorer`).
#[derive(Debug, Default)]
pub(crate) struct NamespaceRestorer {
    refs: NamespaceRefs,
    /// The namespace of each dispatcher call announced, by item ID.
    dispatcher_items: HashMap<String, String>,
}

impl NamespaceRestorer {
    /// A restorer for the request's flattened and folded tool names.
    pub(crate) fn new(refs: NamespaceRefs) -> Self {
        Self {
            refs,
            dispatcher_items: HashMap::new(),
        }
    }

    /// The event with its calls restored (`restore`):
    /// - `response.output_item.added` of a dispatcher call gets the
    ///   namespace (the child isn't known yet), and the item is remembered;
    /// - `response.function_call_arguments.done` of a remembered dispatcher
    ///   call gets the child's arguments;
    /// - any other event's `item` and `response.output` calls are restored
    ///   in full.
    pub(crate) fn restore(&mut self, data: Vec<u8>) -> Vec<u8> {
        if self.refs.is_empty() || data.is_empty() {
            return data;
        }
        let Some(mut event) = parse(&data) else {
            return data;
        };
        if self.restore_event(&mut event) {
            write(&event, data)
        } else {
            data
        }
    }

    fn restore_event(&mut self, event: &mut Value) -> bool {
        match str_of(event.get("type")).as_str() {
            "response.output_item.added" => {
                let Some(item) = event.get_mut("item") else {
                    return false;
                };
                if str_of(item.get("type")) != "function_call" {
                    return false;
                }
                let Some(reference) = self
                    .refs
                    .get(&trimmed(item, "name"))
                    .filter(|reference| reference.is_dispatcher)
                else {
                    return false;
                };
                let id = trimmed(item, "id");
                if !id.is_empty() {
                    self.dispatcher_items
                        .insert(id, reference.namespace.clone());
                }
                set(item, "namespace", Value::from(reference.namespace.clone()))
            }
            "response.function_call_arguments.done" => {
                let Some(namespace) = self.dispatcher_items.get(&trimmed(event, "item_id")) else {
                    return false;
                };
                let arguments = str_of(event.get("arguments"));
                match unwrap_dispatcher_arguments(&arguments, namespace, &self.refs) {
                    Some((_, child_arguments)) => {
                        set(event, "arguments", Value::String(child_arguments))
                    }
                    None => false,
                }
            }
            _ => {
                let mut changed = event
                    .get_mut("item")
                    .is_some_and(|item| self.restore_item(item));
                if let Some(Value::Array(output)) = json::get_mut(event, "response.output") {
                    for item in output {
                        changed |= self.restore_item(item);
                    }
                }
                changed
            }
        }
    }

    /// `restoreAtPath`: a flattened tool's call gets its name and namespace;
    /// a dispatcher's call, the namespace and, when its arguments name one,
    /// the child's name and arguments.
    fn restore_item(&self, item: &mut Value) -> bool {
        if str_of(item.get("type")) != "function_call" {
            return false;
        }
        let Some(reference) = self.refs.get(&trimmed(item, "name")) else {
            return false;
        };
        if !reference.is_dispatcher {
            set(item, "name", Value::from(reference.name.clone()));
            set(item, "namespace", Value::from(reference.namespace.clone()));
            return true;
        }
        let arguments = str_of(item.get("arguments"));
        let (child_name, child_arguments) =
            match unwrap_dispatcher_arguments(&arguments, &reference.namespace, &self.refs) {
                Some((name, arguments)) => (name, Some(arguments)),
                None => (reference.name.clone(), None),
            };
        set(item, "namespace", Value::from(reference.namespace.clone()));
        if !child_name.is_empty() {
            set(item, "name", Value::String(child_name));
        }
        if let Some(arguments) = child_arguments.filter(|arguments| !arguments.is_empty()) {
            set(item, "arguments", Value::String(arguments));
        }
        true
    }
}

/// `unwrapXAIDispatcherArguments`: the child's name and arguments from a
/// dispatcher call's arguments, `{"name": <child>, "arguments": <args>}`;
/// with no `arguments`, the rest of the object is the child's. `None` when
/// the arguments aren't JSON with a string `name`, the name is itself a
/// dispatcher in `namespace`, or, with no namespace, the name isn't a
/// dispatcher's and there are no `arguments`.
fn unwrap_dispatcher_arguments(
    raw: &str,
    namespace: &str,
    refs: &NamespaceRefs,
) -> Option<(String, String)> {
    let mut parsed: Value = serde_json::from_str(raw).ok()?;
    let Some(Value::String(name)) = parsed.get("name") else {
        return None;
    };
    let child = name.trim().to_owned();
    if child.is_empty() {
        return None;
    }
    if !namespace.is_empty() {
        if refs
            .get(&tools::qualify(namespace, &child))
            .is_some_and(|reference| reference.is_dispatcher)
        {
            return None;
        }
    } else {
        let is_dispatcher_child = refs.values().any(|reference| {
            reference.is_dispatcher && (reference.name == child || reference.namespace == child)
        });
        if !is_dispatcher_child && parsed.get("arguments").is_none() {
            return None;
        }
    }
    let arguments = match parsed.get("arguments") {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => {
            json::delete(&mut parsed, "name");
            parsed.to_string()
        }
    };
    let arguments = if arguments.is_empty() {
        "{}".to_owned()
    } else {
        arguments
    };
    Some((child, arguments))
}

/// Renames the item's `paths` from `alias` to `web_search`, unless it has a
/// namespace.
fn restore_web_search_in(item: &mut Value, alias: &str, paths: &[&str]) -> bool {
    if !trimmed(item, "namespace").is_empty() {
        return false;
    }
    let mut changed = false;
    for path in paths {
        if str_of(get(item, path)).trim() == alias {
            changed |= set(item, path, Value::from(tools::WEB_SEARCH));
        }
    }
    changed
}

/// `restoreXAIClientWebSearchName`: gives the client's `web_search`
/// function, renamed `alias` for Grok, its name back in an event's `item`,
/// its `response.output` and `output` items (`name` and `function.name`),
/// and its own `name`. A namespaced call keeps its name.
pub(crate) fn restore_client_web_search_name(data: Vec<u8>, alias: &str) -> Vec<u8> {
    if alias.is_empty() || !contains(&data, alias.as_bytes()) {
        return data;
    }
    let Some(mut event) = parse(&data) else {
        return data;
    };
    const ITEM: &[&str] = &["name", "function.name"];
    let mut changed = event
        .get_mut("item")
        .is_some_and(|item| restore_web_search_in(item, alias, ITEM));
    for path in ["response.output", "output"] {
        if let Some(Value::Array(output)) = json::get_mut(&mut event, path) {
            for item in output {
                changed |= restore_web_search_in(item, alias, ITEM);
            }
        }
    }
    changed |= restore_web_search_in(&mut event, alias, &["name"]);
    if changed { write(&event, data) } else { data }
}

/// Whether `needle` is in `haystack`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// `xaiPatchCompletedOutput`: a completed (or incomplete) response event
/// with its usage details filled in (see
/// [`ensure_responses_usage_details`]) and, when its `output` is missing
/// or empty, the kept `response.output_item.done` items as its `output`,
/// those with an output index first, in order.
pub(crate) fn patch_completed_output(data: Vec<u8>, items: &OutputItems) -> Vec<u8> {
    let data = ensure_responses_usage_details(data);
    if items.len() == 0 {
        return data;
    }
    let Some(mut event) = parse(&data) else {
        return data;
    };
    let has_output = get(&event, "response.output")
        .and_then(Value::as_array)
        .is_some_and(|output| !output.is_empty());
    // With no output, Codex's patch only fills it in.
    if !has_output && items.patch(&mut event) {
        write(&event, data)
    } else {
        data
    }
}

#[cfg(test)]
mod tests;
