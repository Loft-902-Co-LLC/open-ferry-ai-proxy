// Ported from CLIProxyAPI internal/client/codex/tool-schema/tool_schema.go
// (IsCodexUserAgent, NormalizeCodexToolIntegerTypes, matchCodexTargetTool,
// normalizeCodexToolFieldTypes, normalizeToolIntegerTypesInArray,
// normalizeToolIntegerTypesInElement) (v8.0.20, MIT).
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
//! The tools are Codex's own, known by name. A tool in a namespace is known
//! by both names, as `<namespace>__<tool>` (`memories__read`), and a
//! namespace inside another is left alone. A parameter is named by its path
//! from the schema's `properties`, which reaches into nested objects
//! (`barrier.properties.participants`), array items
//! (`open.items.properties.lineno`) and the branches of a union
//! (`start_line.anyOf.0`).
//!
//! Deviations from upstream:
//! - Upstream logs at debug level when it changed a type; this crate doesn't
//!   log, so its callers do.
//! - A `type` array's members are written as strings, as upstream does, but
//!   an object or array among them is written as compact JSON rather than
//!   as the client wrote it.

use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::go;
use crate::json::str_of;

/// The parameters of each Codex tool that take whole numbers
/// (`codexClientToolIntegerFields`). Each is a path from the schema's
/// `properties`, of object keys and array indexes joined by dots, rather than
/// a name looked for at any depth.
const INTEGER_FIELDS: [(&str, &[&str]); 25] = [
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
            "barrier.properties.participants",
            "barrier.properties.timeout_ms",
        ],
    ),
    ("create_goal", &["token_budget"]),
    ("get_channels", &["limit"]),
    ("list_threads", &["limit", "max_chars_per_post"]),
    ("search_posts", &["limit", "max_chars_per_post"]),
    ("read_thread", &["limit", "max_chars_per_post"]),
    ("read_post", &["offset_chars", "limit_chars"]),
    ("memories__list", &["max_results"]),
    ("memories__read", &["line_offset", "max_lines"]),
    ("memories__search", &["context_lines", "max_results"]),
    ("history__list_windows", &["limit"]),
    ("history__list_items", &["limit", "max_chars_per_item"]),
    ("history__read_item", &["offset_chars", "limit_chars"]),
    ("history__search_contents", &["limit"]),
    ("notes__list_files_by_prefix", &["max_results"]),
    (
        "notes__read_file",
        // Codex declares the signed line numbers in a union's first branch.
        &[
            "start_line",
            "stop_line",
            "start_line.anyOf.0",
            "stop_line.anyOf.0",
        ],
    ),
    (
        "notes__search_contents",
        &["max_matches_per_file", "max_files"],
    ),
    ("image_gen__imagegen", &["num_last_images_to_include"]),
    (
        "web__run",
        &[
            "search_query.items.properties.recency",
            "image_query.items.properties.recency",
            "open.items.properties.lineno",
            "click.items.properties.id",
            "screenshot.items.properties.pageno",
            "weather.items.properties.duration",
            "sports.items.properties.num_games",
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
        changed |= normalize_tools(tools, "");
    }
    if let Some(Value::Array(input)) = body.get_mut("input") {
        for item in input {
            if str_of(item.get("type")) == "additional_tools"
                && let Some(tools) = item.get_mut("tools")
            {
                changed |= normalize_tools(tools, "");
            }
        }
    }
    changed
}

/// `normalizeToolIntegerTypesInArray`: the tools of `namespace`, or of no
/// namespace when it is `""`.
fn normalize_tools(tools: &mut Value, namespace: &str) -> bool {
    let Value::Array(tools) = tools else {
        return false;
    };
    let mut changed = false;
    for tool in tools {
        if let Value::Object(tool) = tool {
            changed |= normalize_tool(tool, namespace);
        }
    }
    changed
}

/// `normalizeToolIntegerTypesInElement`.
fn normalize_tool(tool: &mut Map<String, Value>, namespace: &str) -> bool {
    if str_of(tool.get("type")) == "namespace" {
        // Namespaces don't nest, and one without a name names no tool.
        if !namespace.is_empty() {
            return false;
        }
        let namespace = str_of(tool.get("name")).into_owned();
        if namespace.is_empty() {
            return false;
        }
        return tool
            .get_mut("tools")
            .is_some_and(|tools| normalize_tools(tools, &namespace));
    }
    for key in ["function_declarations", "functionDeclarations"] {
        if let Some(declarations @ Value::Array(_)) = tool.get_mut(key) {
            return normalize_tools(declarations, namespace);
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

    if !namespace.is_empty() {
        name = format!("{namespace}__{name}");
    }
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
/// `functions__` or `collab__` prefix (`matchCodexTargetTool`). The
/// `collaboration` and `multi_agent_v1` namespaces' tools are the flat tools
/// of the same names.
fn integer_fields(name: &str) -> Option<&'static [&'static str]> {
    let name = name.trim();
    let name = name
        .strip_prefix("functions__")
        .or_else(|| name.strip_prefix("collab__"))
        .unwrap_or(name);
    let name = match name {
        "multi_agent_v1__wait_agent" | "collaboration__wait_agent" => "wait_agent",
        "collaboration__get_channels"
        | "collaboration__list_threads"
        | "collaboration__search_posts"
        | "collaboration__read_thread"
        | "collaboration__read_post" => name.strip_prefix("collaboration__").unwrap_or(name),
        name => name,
    };
    INTEGER_FIELDS
        .iter()
        .find(|(tool, _)| *tool == name)
        .map(|(_, fields)| *fields)
}

/// `normalizeCodexToolFieldTypes`: declares the schemas at the paths
/// `fields` in `properties` integers, where they are numbers.
fn normalize_properties(properties: &mut Map<String, Value>, fields: &[&str]) -> bool {
    let mut changed = false;
    for field in fields {
        let Some(property) = property_mut(properties, field) else {
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
                let mut seen = HashSet::with_capacity(items.len());
                let mut kinds: Vec<String> = Vec::with_capacity(items.len());
                for item in items.iter() {
                    let mut text = str_of(Some(item)).into_owned();
                    if text == "number" {
                        has_number = true;
                        text = "integer".to_owned();
                    }
                    if seen.insert(text.clone()) {
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

/// The schema object at `path` in `properties`, as gjson reads the path: a
/// dot separates the steps, each a key in an object or an index in an array.
fn property_mut<'v>(
    properties: &'v mut Map<String, Value>,
    path: &str,
) -> Option<&'v mut Map<String, Value>> {
    let mut steps = path.split('.');
    let mut value = properties.get_mut(steps.next()?)?;
    for step in steps {
        value = match value {
            Value::Object(map) => map.get_mut(step)?,
            Value::Array(items) if !step.is_empty() && step.bytes().all(|b| b.is_ascii_digit()) => {
                items.get_mut(step.parse::<usize>().ok()?)?
            }
            _ => return None,
        };
    }
    value.as_object_mut()
}

#[cfg(test)]
mod tests;
