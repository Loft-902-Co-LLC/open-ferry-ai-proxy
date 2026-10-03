// Ported from CLIProxyAPI internal/client/codex/tool-schema/tool_schema.go
// (IsCodexUserAgent, NormalizeCodexToolIntegerTypes, matchCodexTargetTool,
// normalizeCodexToolFieldTypes, normalizeToolIntegerTypesInArray,
// normalizeToolIntegerTypesInElement) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Integer parameter types for Codex's own tools, when a Codex client's
//! request goes to another provider.
//!
//! Codex declares counts and durations such as `exec_command`'s
//! `timeout_ms` as `number`, and other providers' models then send values
//! like `1000.0`, which Codex rejects. [`normalize`] declares those
//! parameters `integer` instead, in the tool formats of each provider:
//! Responses and Chat Completions functions, Claude's `input_schema`,
//! Gemini's function declarations and `parametersJsonSchema`, and tools
//! inside namespaces.
//!
//! Deviations from upstream:
//! - Upstream logs at debug level when it changed a type; this crate doesn't
//!   log, so its callers do.
//! - A `type` array's members are written as strings, as upstream does, but
//!   an object or array among them is written as compact JSON rather than
//!   as the client wrote it.

use serde_json::{Map, Value};

use crate::go;
use crate::json::str_of;

/// The parameters of each Codex tool that take whole numbers
/// (`codexClientToolIntegerFields`).
const INTEGER_FIELDS: [(&str, &[&str]); 7] = [
    (
        "exec_command",
        &["yield_time_ms", "max_output_tokens", "timeout_ms"],
    ),
    (
        "write_stdin",
        &["session_id", "yield_time_ms", "max_output_tokens"],
    ),
    ("sleep", &["duration_ms"]),
    ("wait_agent", &["timeout_ms"]),
    ("wait", &["yield_time_ms", "max_tokens"]),
    ("tool_search", &["limit"]),
    (
        "test_sync_tool",
        &[
            "sleep_before_ms",
            "sleep_after_ms",
            "participants",
            "timeout_ms",
        ],
    ),
];

/// Whether `user_agent` names a Codex client of any kind, by containing
/// `codex` in any case (`IsCodexUserAgent`). This is looser than
/// [`super::multi_agent_v2::is_codex_client_user_agent`].
pub fn is_codex_user_agent(user_agent: &str) -> bool {
    !user_agent.is_empty() && go::to_lower(user_agent).contains("codex")
}

/// Declares Codex's whole-number tool parameters `integer` rather than
/// `number`, in `tools` and in the tools of `additional_tools` input items,
/// when `user_agent` is a Codex client's (`NormalizeCodexToolIntegerTypes`).
/// A `type` array has `number` replaced by `integer`, once. Returns whether
/// the body changed.
pub fn normalize(body: &mut Value, user_agent: &str) -> bool {
    if !is_codex_user_agent(user_agent) {
        return false;
    }
    let mut changed = false;
    if let Some(tools) = body.get_mut("tools") {
        changed |= normalize_tools(tools);
    }
    if let Some(Value::Array(input)) = body.get_mut("input") {
        for item in input {
            if str_of(item.get("type")) == "additional_tools"
                && let Some(tools) = item.get_mut("tools")
            {
                changed |= normalize_tools(tools);
            }
        }
    }
    changed
}

/// `normalizeToolIntegerTypesInArray`.
fn normalize_tools(tools: &mut Value) -> bool {
    let Value::Array(tools) = tools else {
        return false;
    };
    let mut changed = false;
    for tool in tools {
        if let Value::Object(tool) = tool {
            changed |= normalize_tool(tool);
        }
    }
    changed
}

/// `normalizeToolIntegerTypesInElement`.
fn normalize_tool(tool: &mut Map<String, Value>) -> bool {
    if str_of(tool.get("type")) == "namespace" {
        return tool.get_mut("tools").is_some_and(normalize_tools);
    }
    for key in ["function_declarations", "functionDeclarations"] {
        if let Some(declarations @ Value::Array(_)) = tool.get_mut(key) {
            return normalize_tools(declarations);
        }
    }

    let mut name = str_of(tool.get("name")).into_owned();
    let path: &[&str] = if tool.get("parameters").is_some_and(Value::is_object) {
        &["parameters"]
    } else if tool
        .get("function")
        .and_then(|function| function.get("parameters"))
        .is_some_and(Value::is_object)
    {
        if name.is_empty() {
            name = str_of(
                tool.get("function")
                    .and_then(|function| function.get("name")),
            )
            .into_owned();
        }
        &["function", "parameters"]
    } else if tool.get("input_schema").is_some_and(Value::is_object) {
        &["input_schema"]
    } else if tool
        .get("parametersJsonSchema")
        .is_some_and(Value::is_object)
    {
        &["parametersJsonSchema"]
    } else {
        return false;
    };

    let Some(fields) = integer_fields(&name) else {
        return false;
    };
    let mut parameters = tool.get_mut(path[0]);
    for key in &path[1..] {
        parameters = parameters.and_then(|value| value.get_mut(*key));
    }
    match parameters.and_then(|parameters| parameters.get_mut("properties")) {
        Some(Value::Object(properties)) => normalize_properties(properties, fields),
        _ => false,
    }
}

/// The whole-number parameters of the Codex tool `name`, which may have a
/// `functions__` or `collab__` prefix (`matchCodexTargetTool`).
fn integer_fields(name: &str) -> Option<&'static [&'static str]> {
    let name = name.trim();
    let name = name
        .strip_prefix("functions__")
        .or_else(|| name.strip_prefix("collab__"))
        .unwrap_or(name);
    INTEGER_FIELDS
        .iter()
        .find(|(tool, _)| *tool == name)
        .map(|(_, fields)| *fields)
}

/// `normalizeCodexToolFieldTypes`.
fn normalize_properties(properties: &mut Map<String, Value>, fields: &[&str]) -> bool {
    let mut changed = false;
    for field in fields {
        let Some(Value::Object(property)) = properties.get_mut(*field) else {
            continue;
        };
        let Some(kind) = property.get_mut("type") else {
            continue;
        };
        match kind {
            Value::String(text) if text == "number" => {
                *kind = Value::from("integer");
                changed = true;
            }
            Value::Array(items) => {
                let mut has_number = false;
                let mut kinds: Vec<String> = Vec::with_capacity(items.len());
                for item in items.iter() {
                    let mut text = str_of(Some(item)).into_owned();
                    if text == "number" {
                        has_number = true;
                        text = "integer".to_owned();
                    }
                    if !kinds.contains(&text) {
                        kinds.push(text);
                    }
                }
                if has_number {
                    *kind = Value::Array(kinds.into_iter().map(Value::String).collect());
                    changed = true;
                }
            }
            _ => {}
        }
    }
    changed
}

#[cfg(test)]
mod tests;
