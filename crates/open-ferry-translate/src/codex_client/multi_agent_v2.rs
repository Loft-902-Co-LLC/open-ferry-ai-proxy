// Ported from CLIProxyAPI internal/client/codex/optimize-multi-agent-v2/
// optimize_multi_agent_v2.go (IsCodexClientUserAgent,
// PrepareCodexMultiAgentV2Tools, OptimizeCodexMultiAgentV2Request,
// RewriteCodexMultiAgentV2Input, HasCodexMultiAgentV2NamespaceConflict,
// RestoreCodexMultiAgentV2Response and their helpers) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Codex's multi-agent v2 requests, readied for upstreams other than the
//! one Codex clients are written for (`client.codex.optimize-multi-agent-v2`).
//!
//! An official Codex client (by its user agent, see
//! [`is_codex_client_user_agent`]) describes its collaboration tools
//! (`spawn_agent`, `send_message`, `followup_task`) inside a `collaboration`
//! namespace and marks their `message` parameter `encrypted`, and sends what
//! agents said to each other as `agent_message` items with encrypted content.
//! When the setting is on:
//! - [`prepare_tools`], at the Responses API boundary, drops the `encrypted`
//!   marks so the messages arrive readable;
//! - [`optimize`], in the Codex executor, also turns the encrypted content
//!   of `agent_message` items into plain `input_text`, and renames a
//!   `collaboration` namespace holding `spawn_agent` to
//!   `collaboration-optimize`; [`restore_response`] renames it back in what
//!   the upstream answers;
//! - [`rewrite_input`], for other target formats or a credential's
//!   compatibility models, turns `agent_message` items into user messages
//!   and can strip their routing metadata.
//!
//! A request that already uses the `collaboration-optimize` name
//! ([`has_namespace_conflict`]) keeps its namespace.
//!
//! Deviations from upstream:
//! - Only step 1 is ported. Step 2, listing the models `spawn_agent` may
//!   pick in its description, waits for the Codex client model catalog;
//!   descriptions are left as they are, which is what upstream does when it
//!   has no models to list. The Home model fetch isn't ported.
//! - Upstream notes in the Gin context that the Responses handler prepared
//!   the tools, and then doesn't prepare them again. Without step 2 both
//!   ways give the same body, so no such note is kept.
//! - [`restore_response`] reads and writes JSON as Go does, but leaves alone
//!   data nested more than 128 deep, where Go reads up to 10,000, so that
//!   its recursion can't overflow the stack.

mod go_any;

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::go;
use crate::json::str_of;
use go_any::Node;

/// The namespace Codex clients put their collaboration tools in.
pub const COLLABORATION_NAMESPACE: &str = "collaboration";
/// What [`optimize`] renames that namespace to.
pub const OPTIMIZED_COLLABORATION_NAMESPACE: &str = "collaboration-optimize";
/// The optimized namespace as the prefix of a flattened tool name.
const OPTIMIZED_NAME_PREFIX: &str = "collaboration-optimize__";
/// The optimized namespace as the prefix of a dotted tool name.
const OPTIMIZED_DOT_PREFIX: &str = "collaboration-optimize.";

/// The collaboration tools whose `message` parameter may be marked
/// `encrypted` (`codexCollaborationMessageTools`).
const MESSAGE_TOOLS: [&str; 3] = ["spawn_agent", "send_message", "followup_task"];
/// The tool whose namespace is renamed.
const SPAWN_TOOLS: [&str; 1] = ["spawn_agent"];

/// Whether `user_agent` is an official Codex client's
/// (`IsCodexClientUserAgent`): Codex Desktop, the TUI, the CLI or `codex
/// exec`.
pub fn is_codex_client_user_agent(user_agent: &str) -> bool {
    let user_agent = user_agent.trim();
    user_agent.starts_with("Codex Desktop/")
        || user_agent.starts_with("codex-tui/")
        || user_agent == "codex_cli_rs"
        || user_agent.starts_with("codex_cli_rs/")
        || user_agent.starts_with("codex_exec/")
}

/// Readies the collaboration tool definitions of a request at the Responses
/// API boundary, without renaming their namespace
/// (`PrepareCodexMultiAgentV2Tools`): with the setting `enabled` and an
/// official Codex client's `user_agent`, the `encrypted` mark is dropped
/// from the `message` parameter of each collaboration tool. Returns whether
/// the body changed.
pub fn prepare_tools(body: &mut Value, user_agent: &str, enabled: bool) -> bool {
    if !enabled || !is_codex_client_user_agent(user_agent) {
        return false;
    }
    // Step 2 would also list models in `spawn_agent`'s description, unless
    // the namespace conflicts; without it both cases only drop the marks.
    remove_message_encryption(body)
}

/// Rewrites an eligible request for Codex (`OptimizeCodexMultiAgentV2Request`):
/// with the setting `enabled` and an official Codex client's `user_agent`,
/// encrypted `agent_message` content becomes `input_text`, the collaboration
/// tools lose their `encrypted` marks, and a `collaboration` namespace that
/// holds `spawn_agent` is renamed `collaboration-optimize`, unless the
/// request already uses that name. Returns whether a namespace was renamed,
/// so the response must be restored with [`restore_response`].
pub fn optimize(body: &mut Value, user_agent: &str, enabled: bool) -> bool {
    if !enabled || !is_codex_client_user_agent(user_agent) {
        return false;
    }
    rewrite_agent_message_content(body);
    remove_message_encryption(body);
    if has_namespace_conflict(body) {
        return false;
    }
    rename_collaboration_namespaces(body)
}

/// Turns Codex's `agent_message` input items into plain user messages
/// (`RewriteCodexMultiAgentV2Input`). With `compat`, for a credential's
/// compatibility model, or with the setting `enabled` and an official Codex
/// client's `user_agent`, each `agent_message` gets its encrypted content as
/// `input_text`, the `user` role and the `message` type. With `compat`, every
/// input item also loses `author`, `recipient` and
/// `internal_chat_message_metadata_passthrough`. Returns whether the body
/// changed.
pub fn rewrite_input(body: &mut Value, user_agent: &str, enabled: bool, compat: bool) -> bool {
    let optimize = compat || (enabled && is_codex_client_user_agent(user_agent));
    if !optimize {
        return false;
    }
    rewrite_agent_message_input(body, optimize, compat)
}

/// Whether the request already uses the optimized namespace's name, for a
/// tool or namespace in `tools` or in an `additional_tools` input item
/// (`HasCodexMultiAgentV2NamespaceConflict`).
pub fn has_namespace_conflict(body: &Value) -> bool {
    if tools_conflict(body.get("tools")) {
        return true;
    }
    let Some(Value::Array(input)) = body.get("input") else {
        return false;
    };
    input
        .iter()
        .any(|item| is_additional_tools(item) && tools_conflict(item.get("tools")))
}

/// `codexToolsHaveOptimizedCollaborationConflict`.
fn tools_conflict(tools: Option<&Value>) -> bool {
    let Some(Value::Array(tools)) = tools else {
        return false;
    };
    tools.iter().any(|tool| {
        let name = str_of(tool.get("name"));
        let name = name.trim();
        name == OPTIMIZED_COLLABORATION_NAMESPACE
            || name.starts_with(OPTIMIZED_NAME_PREFIX)
            || name.starts_with(OPTIMIZED_DOT_PREFIX)
            || (str_of(tool.get("type")).trim() == "namespace" && tools_conflict(tool.get("tools")))
    })
}

/// Whether `item` is an `additional_tools` input item.
fn is_additional_tools(item: &Value) -> bool {
    str_of(item.get("type")).trim() == "additional_tools"
}

/// Calls `visit` with each function tool named in `names`, in `tools` and
/// in `additional_tools` input items, also inside namespaces, along with the
/// namespace that holds it, if any (`codexToolPathsByNames`).
fn for_each_tool(
    body: &mut Value,
    names: &[&str],
    visit: &mut impl FnMut(&mut Map<String, Value>, Option<&mut Map<String, Value>>),
) {
    if let Some(tools) = body.get_mut("tools") {
        visit_tools(tools, None, names, visit);
    }
    if let Some(Value::Array(input)) = body.get_mut("input") {
        for item in input {
            if is_additional_tools(item)
                && let Some(tools) = item.get_mut("tools")
            {
                visit_tools(tools, None, names, visit);
            }
        }
    }
}

/// `collectCodexToolPathsByNames` for the tools in `tools`, which
/// `namespace` holds when it isn't `None`.
fn visit_tools(
    tools: &mut Value,
    mut namespace: Option<&mut Map<String, Value>>,
    names: &[&str],
    visit: &mut impl FnMut(&mut Map<String, Value>, Option<&mut Map<String, Value>>),
) {
    let Value::Array(tools) = tools else {
        return;
    };
    for tool in tools {
        let Value::Object(tool) = tool else {
            continue;
        };
        match str_of(tool.get("type")).trim() {
            "function" => {
                if names.contains(&str_of(tool.get("name")).trim()) {
                    visit(tool, namespace.as_deref_mut());
                }
            }
            "namespace" => {
                // The namespace's own fields and its tools are borrowed
                // apart, the tools left in their place as null meanwhile.
                let Some(slot) = tool.get_mut("tools") else {
                    continue;
                };
                let mut nested = std::mem::take(slot);
                visit_tools(&mut nested, Some(tool), names, visit);
                if let Some(slot) = tool.get_mut("tools") {
                    *slot = nested;
                }
            }
            _ => {}
        }
    }
}

/// Drops `parameters.properties.message.encrypted` from each collaboration
/// tool (`removeCodexCollaborationMessageEncryption`). Returns whether any
/// was dropped.
fn remove_message_encryption(body: &mut Value) -> bool {
    let mut changed = false;
    for_each_tool(body, &MESSAGE_TOOLS, &mut |tool, _| {
        if let Some(Value::Object(message)) = tool
            .get_mut("parameters")
            .and_then(|parameters| parameters.get_mut("properties"))
            .and_then(|properties| properties.get_mut("message"))
            && message.shift_remove("encrypted").is_some()
        {
            changed = true;
        }
    });
    changed
}

/// Renames each `collaboration` namespace that directly holds `spawn_agent`
/// (`optimizeCodexCollaborationNamespace`). Returns whether one was renamed.
fn rename_collaboration_namespaces(body: &mut Value) -> bool {
    let mut optimized = false;
    for_each_tool(body, &SPAWN_TOOLS, &mut |_, namespace| {
        if let Some(namespace) = namespace
            && str_of(namespace.get("name")).trim() == COLLABORATION_NAMESPACE
        {
            namespace.insert(
                "name".to_owned(),
                Value::from(OPTIMIZED_COLLABORATION_NAMESPACE),
            );
            optimized = true;
        }
    });
    optimized
}

/// Turns the encrypted content parts of `agent_message` input items into
/// `input_text` parts (`rewriteCodexAgentMessageContent`). Returns whether
/// any changed.
fn rewrite_agent_message_content(body: &mut Value) -> bool {
    let Some(Value::Array(input)) = body.get_mut("input") else {
        return false;
    };
    let mut changed = false;
    for item in input {
        if str_of(item.get("type")).trim() != "agent_message" {
            continue;
        }
        let Some(Value::Array(content)) = item.get_mut("content") else {
            continue;
        };
        for part in content {
            if str_of(part.get("type")).trim() != "encrypted_content" {
                continue;
            }
            let Value::Object(part) = part else {
                continue;
            };
            let Some(Value::String(text)) = part.get("encrypted_content") else {
                continue;
            };
            let text = Value::String(text.clone());
            part.insert("type".to_owned(), Value::from("input_text"));
            part.insert("text".to_owned(), text);
            part.shift_remove("encrypted_content");
            changed = true;
        }
    }
    changed
}

/// `rewriteCodexAgentMessageInput`.
fn rewrite_agent_message_input(body: &mut Value, optimize: bool, compat: bool) -> bool {
    if !matches!(body.get("input"), Some(Value::Array(_))) {
        return false;
    }
    let mut changed = optimize && rewrite_agent_message_content(body);
    let Some(Value::Array(input)) = body.get_mut("input") else {
        return changed;
    };
    for item in input {
        let Value::Object(item) = item else {
            continue;
        };
        if optimize && str_of(item.get("type")).trim() == "agent_message" {
            item.insert("role".to_owned(), Value::from("user"));
            item.insert("type".to_owned(), Value::from("message"));
            changed = true;
        }
        if compat {
            for field in [
                "author",
                "recipient",
                "internal_chat_message_metadata_passthrough",
            ] {
                changed |= item.shift_remove(field).is_some();
            }
        }
    }
    changed
}

/// Renames the optimized namespace back to `collaboration` in one of the
/// upstream's events, or a compact call's answer, when [`optimize`] renamed
/// it (`RestoreCodexMultiAgentV2Response`).
///
/// A tool call's `namespace` of `collaboration-optimize` becomes
/// `collaboration`; a tool call named `collaboration-optimize.<tool>` gets
/// the `collaboration` namespace and the name `<tool>`, and one named
/// `collaboration-optimize__<tool>` becomes `collaboration__<tool>`; and a
/// namespace named `collaboration-optimize` becomes `collaboration`. Tool
/// arguments, custom tool input and tool outputs are left as they are.
///
/// As upstream re-encodes changed data with Go's `json.Marshal`, so does
/// this: object keys sorted, `<`, `>`, `&`, U+2028 and U+2029 escaped, and
/// numbers as written. Data that isn't valid JSON, or needs no change, is
/// returned as it is.
pub fn restore_response(data: &[u8], optimized: bool) -> Cow<'_, [u8]> {
    if !optimized || data.is_empty() || !go::gjson_valid(data) {
        return Cow::Borrowed(data);
    }
    let Some(mut node) = Node::parse(data) else {
        return Cow::Borrowed(data);
    };
    if !restore_node(&mut node) {
        return Cow::Borrowed(data);
    }
    let mut out = String::with_capacity(data.len());
    node.write(&mut out);
    Cow::Owned(out.into_bytes())
}

/// `restoreCodexCollaborationValue`. Returns whether anything changed.
fn restore_node(node: &mut Node<'_>) -> bool {
    match node {
        Node::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= restore_node(item);
            }
            changed
        }
        Node::Object(fields) => {
            let item_type = fields
                .get("type")
                .and_then(Node::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned();
            let is_tool_call = item_type == "function_call" || item_type == "custom_tool_call";
            let mut changed = false;
            let mut set = |fields: &mut BTreeMap<String, Node<'_>>, key: &str, text: String| {
                fields.insert(key.to_owned(), Node::String(text));
                changed = true;
            };
            if is_tool_call
                && fields.get("namespace").and_then(Node::as_str)
                    == Some(OPTIMIZED_COLLABORATION_NAMESPACE)
            {
                set(fields, "namespace", COLLABORATION_NAMESPACE.to_owned());
            }
            if let Some(name) = fields.get("name").and_then(Node::as_str) {
                if name == OPTIMIZED_COLLABORATION_NAMESPACE && item_type == "namespace" {
                    set(fields, "name", COLLABORATION_NAMESPACE.to_owned());
                } else if let Some(tool) = name.strip_prefix(OPTIMIZED_DOT_PREFIX)
                    && is_tool_call
                {
                    if !tool.is_empty() {
                        let tool = tool.to_owned();
                        set(fields, "namespace", COLLABORATION_NAMESPACE.to_owned());
                        set(fields, "name", tool);
                    }
                } else if let Some(tool) = name.strip_prefix(OPTIMIZED_NAME_PREFIX)
                    && is_tool_call
                {
                    let name = format!("{COLLABORATION_NAMESPACE}__{tool}");
                    set(fields, "name", name);
                }
            }
            let is_output =
                item_type == "function_call_output" || item_type == "custom_tool_call_output";
            for (key, child) in fields.iter_mut() {
                if key == "arguments" || key == "input" || (key == "output" && is_output) {
                    continue;
                }
                changed |= restore_node(child);
            }
            changed
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests;
