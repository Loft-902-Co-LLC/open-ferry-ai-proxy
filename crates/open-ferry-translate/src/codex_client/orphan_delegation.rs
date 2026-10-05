// Ported from CLIProxyAPI internal/client/codex/optimize-multi-agent-v2/
// orphan_delegation.go (RewriteCodexOrphanDelegationInput,
// isCodexCollabSpawnSubagent, matchCodexDelegationTool,
// buildCodexOrphanUserMessage) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Delegation tool outputs whose call isn't in the request, as user messages
//! (`codex.orphan-delegation-compatibility`).
//!
//! A Codex sub-agent (sent with `X-Openai-Subagent: collab_spawn`) starts
//! with the output of the `codex_app` tool that delegated to it, but not the
//! call that made it, which most upstreams reject. With the setting on,
//! each `function_call_output` of `codex_app`'s `create_thread` or
//! `send_message_to_thread` that no `function_call` in the request answers
//! becomes a user message quoting the output.
//!
//! Deviations from upstream:
//! - An output that is neither a string nor missing is quoted as compact
//!   JSON, as serde_json writes it, rather than as the client wrote it.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::go;
use crate::json::str_of;

/// The header that names the kind of sub-agent a Codex request is for.
pub const SUBAGENT_HEADER: &str = "x-openai-subagent";

/// The [`SUBAGENT_HEADER`] value of a delegated sub-agent.
const COLLAB_SPAWN: &str = "collab_spawn";

/// Rewrites a Responses request's orphan delegation outputs
/// (`RewriteCodexOrphanDelegationInput`): with the setting `enabled` and a
/// `subagent` header (see [`super::header_value`]) of `collab_spawn` in any
/// case, each `function_call_output` of `codex_app`'s `create_thread` or
/// `send_message_to_thread` that no `function_call` with its `call_id`
/// answers becomes a user message. Each `function_call` answers one output,
/// wherever in the input it is. Returns whether the body changed.
pub fn rewrite(body: &mut Value, subagent: &str, enabled: bool) -> bool {
    if !enabled || !go::equal_fold(subagent, COLLAB_SPAWN) {
        return false;
    }
    let Some(Value::Array(input)) = body.get_mut("input") else {
        return false;
    };
    let mut available: HashMap<String, usize> = HashMap::new();
    for item in input.iter() {
        if str_of(item.get("type")) != "function_call" {
            continue;
        }
        let call_id = str_of(item.get("call_id"));
        if !call_id.trim().is_empty() {
            *available.entry(call_id.into_owned()).or_default() += 1;
        }
    }

    let mut changed = false;
    for item in input.iter_mut() {
        if str_of(item.get("type")) != "function_call_output" {
            continue;
        }
        let call_id = str_of(item.get("call_id"));
        if !call_id.trim().is_empty()
            && let Some(count) = available.get_mut(&*call_id).filter(|count| **count > 0)
        {
            // Answered by a call in the same request.
            *count -= 1;
            continue;
        }
        let Some(label) = delegation_tool(item) else {
            continue;
        };
        *item = user_message(label, item.get("output"));
        changed = true;
    }
    changed
}

/// The name of the delegation tool whose output `item` is, if it is one
/// (`matchCodexDelegationTool`).
fn delegation_tool(item: &Value) -> Option<&'static str> {
    if str_of(item.get("namespace")) != "codex_app" {
        return None;
    }
    match &*str_of(item.get("name")) {
        "create_thread" => Some("codex_app__create_thread"),
        "send_message_to_thread" => Some("codex_app__send_message_to_thread"),
        _ => None,
    }
}

/// The user message that quotes a delegation tool's `output`
/// (`buildCodexOrphanUserMessage`).
fn user_message(label: &str, output: Option<&Value>) -> Value {
    let output = match output {
        None => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    };
    json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": format!("Tool output from {label}:\n{output}")}],
    })
}

#[cfg(test)]
mod tests;
