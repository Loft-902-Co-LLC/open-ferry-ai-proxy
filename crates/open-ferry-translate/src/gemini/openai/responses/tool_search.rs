// Ported from CLIProxyAPI internal/translator/gemini/openai/responses/gemini_openai-responses_tool_search.go (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Client tool-search history as Gemini function calls and results.
//!
//! Deviations from upstream:
//! - Search-call argument objects are serialized compactly; upstream keeps
//!   their original JSON text.
//! - A discovery summary's error value is serialized compactly; upstream
//!   keeps its original JSON text.
//! - Summary statuses are written with serde_json's escaping. Upstream's
//!   sjson writes a plain ASCII status as it is, and escapes `<`, `>`, `&`,
//!   U+2028 and U+2029 only in a status that also holds a control
//!   character, a non-ASCII character, a quote or a backslash; serde_json
//!   never escapes those. The decoded status is the same.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::common::responses::extract_responses_call_id;
use crate::json::str_of;
use crate::responses_tools::{
    is_client_search_item, map_client_search_name, map_responses_tool_name,
    qualify_namespace_tool_name,
};

/// `normalizeOpenAIResponsesToolSearchItems`: replay search history through
/// the request translator's existing function-call pairing.
pub(super) fn normalize_items<'a>(
    items: &'a [Value],
    forward: &HashMap<String, String>,
) -> Cow<'a, [Value]> {
    if !items.iter().any(|item| {
        matches!(
            &*str_of(item.get("type")),
            "tool_search_call" | "tool_search_output"
        )
    }) {
        return Cow::Borrowed(items);
    }
    Cow::Owned(
        items
            .iter()
            .filter_map(|item| normalize_item(item, forward))
            .collect(),
    )
}

fn normalize_item(item: &Value, forward: &HashMap<String, String>) -> Option<Value> {
    let kind = str_of(item.get("type"));
    if !matches!(&*kind, "tool_search_call" | "tool_search_output") {
        return Some(item.clone());
    }
    if !is_client_search_item(item) {
        return None;
    }
    let mut out = json!({
        "type": if kind == "tool_search_call" { "function_call" } else { "function_call_output" },
        "name": map_client_search_name(forward),
        "call_id": extract_responses_call_id(item),
    });
    if let Some(id) = item.get("id") {
        out["id"] = str_of(Some(id)).into_owned().into();
    }
    if kind == "tool_search_call" {
        if let Some(status) = item.get("status") {
            out["status"] = str_of(Some(status)).into_owned().into();
        }
        out["arguments"] = match item.get("arguments") {
            Some(Value::String(text)) => text.clone(),
            Some(value @ (Value::Object(_) | Value::Array(_))) => value.to_string(),
            _ => "{}".into(),
        }
        .into();
    } else {
        let status = str_of(item.get("status"));
        let status = if status.trim().is_empty() {
            "completed"
        } else {
            status.trim()
        };
        let mut summary = json!({
            "status": status,
            "tools": discovered_names(item.get("tools"), forward),
        });
        if let Some(error) = item.get("error").filter(|error| !error.is_null()) {
            summary["error"] = error.clone();
        }
        out["output"] = summary.to_string().into();
    }
    Some(out)
}

fn discovered_names(tools: Option<&Value>, forward: &HashMap<String, String>) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    let name_of = |v: &Value| {
        let name = str_of(v.get("name"));
        if name.trim().is_empty() {
            str_of(v.get("function").and_then(|f| f.get("name")))
                .trim()
                .to_owned()
        } else {
            name.trim().to_owned()
        }
    };
    let mut append = |name: String| {
        if !name.trim().is_empty() {
            let mapped = map_responses_tool_name(forward, name.trim());
            if seen.insert(mapped.clone()) {
                names.push(mapped);
            }
        }
    };
    for tool in tools.and_then(Value::as_array).into_iter().flatten() {
        match str_of(tool.get("type")).trim() {
            "" | "function" | "custom" => append(name_of(tool)),
            "namespace" => {
                let ns = str_of(tool.get("name"));
                let children = tool
                    .get("tools")
                    .filter(|v| v.is_array())
                    .or_else(|| tool.get("children"));
                let mut append_child = |child: &Value| {
                    let name = name_of(child);
                    if !name.is_empty() {
                        append(qualify_namespace_tool_name(&ns, &name));
                    }
                };
                match children {
                    Some(Value::Array(children)) => children.iter().for_each(&mut append_child),
                    Some(Value::Object(children)) => children.values().for_each(&mut append_child),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    names
}

pub(super) fn call_item(call_id: &str, status: &str, arguments: &str) -> Value {
    let args = crate::json::exact::from_str(arguments)
        .ok()
        .filter(Value::is_object);
    let status = if status == "completed" && args.is_none() {
        "incomplete"
    } else {
        status
    };
    let mut item = json!({"id":format!("tsc_{call_id}"),"type":"tool_search_call","status":status,
        "execution":"client","call_id":call_id});
    item["arguments"] = args.unwrap_or_else(|| json!({}));
    item
}

#[cfg(test)]
mod tests;
