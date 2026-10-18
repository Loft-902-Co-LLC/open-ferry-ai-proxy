// Ported from CLIProxyAPI internal/translator/claude/openai/responses/claude_openai-responses_tool_search.go (v8.0.32, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Codex's client search tool and the deferred tools its results discover.

use std::collections::HashSet;

use serde_json::{Value, json};

use super::tools::{RequestTools, discovered_descriptors, tool_description};
use crate::json::str_of;
use crate::schema::normalize_claude_tool_input_schema;

pub(super) const TYPE: &str = "tool_search";
pub(super) const DISCOVERED_PRIORITY: u8 = 2;

pub(super) fn is_client_search(tool: &Value) -> bool {
    str_of(tool.get("type")).trim() == TYPE && str_of(tool.get("execution")).trim() == "client"
}

pub(super) fn declaration(tool: &Value, name: &str) -> Value {
    let mut schema = tool
        .get("parameters")
        .filter(|v| v.is_object())
        .map_or_else(
            || {
                json!({"type":"object","properties":{
            "query":{"type":"string","description":"Search query for deferred tools."},
            "limit":{"type":"number","description":"Maximum number of tools to return."}
        },"required":["query"],"additionalProperties":false})
            },
            |schema| normalize_claude_tool_input_schema(Some(schema)),
        );
    if schema["properties"]["query"]["type"] != "string" {
        schema["properties"]["query"] =
            json!({"type":"string","description":"Search query for deferred tools."});
    }
    if !schema["required"]
        .as_array()
        .is_some_and(|a| a.iter().any(|v| v == "query"))
    {
        if !schema["required"].is_array() {
            schema["required"] = json!([]);
        }
        schema["required"]
            .as_array_mut()
            .expect("array")
            .push("query".into());
    }
    json!({"name":name,"description":tool_description(tool),"input_schema":schema})
}

/// Names the constant deferred tool declared with native tool search; nothing
/// ever references it.
pub(super) const DEFERRED_ANCHOR_NAME: &str = "tool_search_deferred_anchor";

/// `responsesDeferredAnchorTool`: Anthropic keeps the prompt cache when
/// deferred tools are added, but not when a request goes from no deferred tool
/// to one. This anchor is declared from the first request on, so discoveries
/// only append after it. `None` when a converted tool already carries its name
/// (names are normalized during conversion, so the check runs on the final
/// tools); the request then keeps the client's tool and has no anchor.
pub(super) fn deferred_anchor_tool(claude_tools: &[Value]) -> Option<Value> {
    if claude_tools
        .iter()
        .any(|tool| str_of(tool.get("name")) == DEFERRED_ANCHOR_NAME)
    {
        return None;
    }
    Some(json!({
        "name": DEFERRED_ANCHOR_NAME,
        "description": "Placeholder that is never loaded; tool search returns the real tools.",
        "input_schema": {"type": "object", "properties": {}},
        "defer_loading": true
    }))
}

pub(super) fn output_text(item: &Value, tools: &RequestTools) -> String {
    let names: Vec<_> = discovered_descriptors(item.get("tools"))
        .iter()
        .map(|d| tools.claude_name(&d.name))
        .collect();
    if names.is_empty() {
        "No matching tools were found.".into()
    } else {
        format!("Tools now available to call: {}.", names.join(", "))
    }
}

pub(super) fn output_content(item: &Value, tools: &RequestTools) -> Value {
    if tools.client_search_wins() {
        let mut seen = HashSet::new();
        let references: Vec<_> = discovered_descriptors(item.get("tools"))
            .iter()
            .filter(|d| {
                tools.winner(&d.name).is_some_and(|w| {
                    w.source_priority == DISCOVERED_PRIORITY
                        && matches!(w.kind.as_str(), "function" | "custom")
                })
            })
            .map(|d| tools.claude_name(&d.name))
            .filter(|name| seen.insert(name.clone()))
            .map(|name| json!({"type":"tool_reference","tool_name":name}))
            .collect();
        if !references.is_empty() {
            return references.into();
        }
    }
    output_text(item, tools).into()
}

pub(super) fn call_status(status: &'static str, arguments: &str) -> &'static str {
    if status == "completed" && arguments_object(arguments).is_none() {
        "incomplete"
    } else {
        status
    }
}

fn arguments_object(arguments: &str) -> Option<Value> {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .filter(Value::is_object)
}

pub(super) fn call_item(call_id: &str, status: &str, arguments: &str) -> Value {
    json!({"id":format!("tsc_{call_id}"),"type":"tool_search_call","status":status,
        "call_id":call_id,"execution":"client",
        "arguments":arguments_object(arguments).unwrap_or_else(|| json!({}))})
}
