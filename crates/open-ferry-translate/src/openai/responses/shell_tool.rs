// Ported from CLIProxyAPI internal/translator/openai/openai/responses/shell_tool.go
// (convertResponsesShellToolToOpenAIChat, shellHistory, shellCallItem,
// shellCallPlaceholder, responsesToolInputFailure) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The client's local shell, offered to a Chat Completions model as a
//! function.
//!
//! A `shell` tool whose environment is `{"type": "local"}`, declared outside
//! any namespace, becomes a function named `__cpa_local_shell`, or
//! `__cpa_local_shell_1` and so on when another tool has that name (see
//! [`super::tools`]). Its arguments are the shell action: `commands`, each a
//! whole shell command, and optionally `timeout_ms` and `max_output_length`.
//! A shell in any other environment, such as a container, is left out, as
//! other built-in tools are. A `shell` tool choice forces the function.
//!
//! In the input, a local `shell_call` becomes a call to the function whose
//! arguments are its `action`, and a `shell_call_output` becomes that call's
//! output, the whole item as JSON text so that nothing in it is lost. A
//! request that no longer declares the shell still replays its calls, under a
//! name no tool or call in the request has.
//!
//! In the response, a call to the function becomes a `shell_call` item whose
//! `action` is the call's arguments, if they are a valid action: an object
//! holding a nonempty list of commands that aren't blank, and limits that are
//! positive whole numbers or `null`, and nothing else. Otherwise the response
//! fails, with a message that says the action was invalid.
//!
//! Deviations from upstream:
//! - A `shell_call`'s `action` is written as compact JSON, where upstream
//!   copies the call's arguments as written. The JSON values are the same.
//! - Arguments `serde_json` can't read are not a valid action. Of those gjson
//!   reads, only arguments whose commands hold an unpaired surrogate escape
//!   are one upstream, which reads it as U+FFFD.
//! - Where a key appears twice in the arguments, the last one counts. gjson
//!   checks the first `commands` but every key, and upstream's `action` keeps
//!   both.

use std::borrow::Cow;
use std::collections::HashSet;

use serde_json::{Map, Value, json};

use super::tool_index::ToolNames;
use crate::apply_patch::input::failure;
use crate::go;
use crate::json::{exact, go_value, object, path, set_path, str_of};

/// The shell function's name, unless another tool has it.
pub(super) const SHELL_NAME: &str = "__cpa_local_shell";

/// `errInvalidShellAction`: why a response whose shell call's arguments
/// aren't a valid action failed.
pub(super) const INVALID_ACTION: &str =
    "invalid shell action: expected nonempty commands strings and optional positive integer limits";

/// Whether a `shell` tool declared in `namespace`, `""` for none, is the
/// client's local shell.
pub(super) fn is_local_shell(tool: &Value, namespace: &str) -> bool {
    namespace.is_empty() && str_of(path(tool, "environment.type")) == "local"
}

/// [`SHELL_NAME`], or the first of `__cpa_local_shell_1`,
/// `__cpa_local_shell_2`, … that `reserved` doesn't hold.
pub(super) fn unreserved_name(reserved: &HashSet<String>) -> String {
    if !reserved.contains(SHELL_NAME) {
        return SHELL_NAME.to_owned();
    }
    (1_u64..)
        .map(|suffix| format!("{SHELL_NAME}_{suffix}"))
        .find(|name| !reserved.contains(name))
        .unwrap_or_else(|| SHELL_NAME.to_owned())
}

/// `convertResponsesShellToolToOpenAIChat`: the Chat Completions function
/// that stands for the local shell, named `name`. Its keys are sorted, as Go
/// writes the map upstream reads it into.
pub(super) fn chat_tool(name: &str) -> Value {
    go_value(&json!({
        "type": "function",
        "function": {
            "name": name,
            "description": "Request commands to execute in the client-provided local shell environment. Each commands entry is a complete shell command, not an argv element.",
            "parameters": {
                "type": "object",
                "properties": {
                    "commands": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                    "timeout_ms": {"type": "integer", "minimum": 1},
                    "max_output_length": {"type": "integer", "minimum": 1},
                },
                "required": ["commands"],
                "additionalProperties": false,
            },
        },
    }))
}

/// `shellHistory`: `items` with each local `shell_call` made a
/// `function_call` to the shell function, its `action` as the arguments, and
/// each `shell_call_output` made a `function_call_output` whose output is the
/// whole item. A `shell_call` in another environment is kept as it is.
pub(super) fn history<'v>(items: &'v [Value], tools: &ToolNames) -> Cow<'v, [Value]> {
    let converts = |item: &Value| match &*str_of(item.get("type")) {
        "shell_call" => is_local_call(item),
        "shell_call_output" => true,
        _ => false,
    };
    if !items.iter().any(converts) {
        return Cow::Borrowed(items);
    }
    let name = match tools.shell_name() {
        "" => replay_name(items, tools),
        name => name.to_owned(),
    };
    let converted = items.iter().map(|item| {
        let mut converted = item.clone();
        let Some(fields) = converted.as_object_mut() else {
            return converted;
        };
        match &*str_of(item.get("type")) {
            "shell_call" if is_local_call(item) => {
                let arguments = item
                    .get("action")
                    .map_or_else(String::new, Value::to_string);
                fields.insert("type".into(), "function_call".into());
                fields.insert("name".into(), name.as_str().into());
                fields.insert("arguments".into(), arguments.into());
            }
            "shell_call_output" => {
                fields.insert("type".into(), "function_call_output".into());
                fields.insert("output".into(), item.to_string().into());
            }
            _ => {}
        }
        converted
    });
    Cow::Owned(converted.collect())
}

/// Whether a `shell_call` ran in the local environment, as one that names no
/// environment did.
fn is_local_call(item: &Value) -> bool {
    path(item, "environment.type").is_none_or(|kind| str_of(Some(kind)) == "local")
}

/// The name the shell's calls are replayed under when the request doesn't
/// declare it: one that no tool has, by any of its names, and that no
/// function or custom tool call in `items` is to.
fn replay_name(items: &[Value], tools: &ToolNames) -> String {
    let mut reserved: HashSet<String> = tools.known_names().map(str::to_owned).collect();
    for item in items {
        if matches!(
            &*str_of(item.get("type")),
            "function_call" | "custom_tool_call"
        ) {
            reserved.insert(tools.canonical_name(&str_of(item.get("name"))));
        }
    }
    unreserved_name(&reserved)
}

/// `shellCallItem`: the `shell_call` item, with `status`, for a call whose
/// `arguments` are a valid action, or `None` if they aren't one.
pub(super) fn call_item(call_id: &str, arguments: &str, status: &str) -> Option<Value> {
    let Ok(Value::Object(action)) = exact::from_str(arguments) else {
        return None;
    };
    if !valid_action(&action) {
        return None;
    }
    let mut item = placeholder(call_id);
    if let Some(fields) = item.as_object_mut() {
        fields.insert("status".into(), status.into());
        fields.insert("action".into(), Value::Object(action));
    }
    Some(item)
}

/// Whether `action` has a nonempty list of `commands`, each a string that
/// isn't blank, and nothing else but a `timeout_ms` and `max_output_length`
/// that are `null` or positive whole numbers.
fn valid_action(action: &Map<String, Value>) -> bool {
    let commands = match action.get("commands") {
        Some(Value::Array(commands)) if !commands.is_empty() => commands,
        _ => return false,
    };
    let command =
        |command: &Value| matches!(command, Value::String(text) if !text.trim().is_empty());
    let limit = |value: &Value| match value {
        Value::Null => true,
        Value::Number(number) => {
            // gjson's `Float()`: infinite past `f64`'s range, which counts.
            let float = go::parse_float(&number.to_string());
            float > 0.0 && float.trunc() == float
        }
        _ => false,
    };
    commands.iter().all(command)
        && action.iter().all(|(key, value)| match key.as_str() {
            "commands" => true,
            "timeout_ms" | "max_output_length" => limit(value),
            _ => false,
        })
}

/// `shellCallPlaceholder`: the `shell_call` item announced before the call's
/// action is known.
pub(super) fn placeholder(call_id: &str) -> Value {
    object([
        ("id", format!("sh_{call_id}").into()),
        ("type", "shell_call".into()),
        ("status", "in_progress".into()),
        ("call_id", call_id.into()),
        ("action", json!({"commands": []})),
    ])
}

/// `responsesToolInputFailure`: the `response.failed` event that ends a
/// response whose tool call input was invalid. It says nothing about the
/// input, unless a shell action was invalid.
pub(super) fn tool_input_failure(response_id: &str, sequence: i64, invalid_action: bool) -> Value {
    let mut event = failure(response_id, sequence);
    if invalid_action {
        set_path(&mut event, "response.error.message", INVALID_ACTION.into());
    }
    event
}

#[cfg(test)]
mod tests;
