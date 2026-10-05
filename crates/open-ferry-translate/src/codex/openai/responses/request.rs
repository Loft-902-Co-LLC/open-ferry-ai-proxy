// Ported from CLIProxyAPI internal/translator/codex/openai/responses/codex_openai-responses_request.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! OpenAI Responses request → Codex request.
//!
//! Codex speaks the Responses API, so the request mostly passes through. We set
//! the fields Codex requires, drop the ones it rejects, and rename what it
//! spells differently. `include` asks for the encrypted reasoning, and keeps
//! asking for web search sources when the client did. The request is edited in
//! place, so a body that needs no changes comes back without being copied.
//!
//! Deviations from upstream:
//! - `prompt_cache_breakpoint` is removed from `input` even when the key is
//!   written with escapes. Upstream skips the pass unless the body contains
//!   the literal text `"prompt_cache_breakpoint"`, so an escaped key reaches
//!   Codex.

use serde_json::{Map, Value, json};

use crate::go;
use crate::json::str_of;

/// Top-level fields Codex rejects.
const UNSUPPORTED_FIELDS: &[&str] = &[
    "max_output_tokens",
    "max_completion_tokens",
    "temperature",
    "top_p",
    "truncation",
    "prompt_cache_options",
    "prompt_cache_retention",
    "context_management",
    "user",
];

const CACHE_BREAKPOINT: &str = "prompt_cache_breakpoint";

/// The `include` entry asking for encrypted reasoning, which Codex requires.
const ENCRYPTED_REASONING: &str = "reasoning.encrypted_content";

/// The `include` entry asking for a web search call's sources.
const WEB_SEARCH_SOURCES: &str = "web_search_call.action.sources";

/// Converts a Responses request into the request Codex expects. `model` is
/// unused: the executor sets the upstream model.
pub fn convert_openai_responses_request_to_codex(_model: &str, mut request: Value) -> Value {
    // sjson can't set a key in an array, so upstream returns one unchanged.
    // Setting a key on any other non-object replaces it with a new object.
    if !request.is_array() && !request.is_object() {
        request = Value::Object(Map::new());
    }
    let Value::Object(fields) = &mut request else {
        return request;
    };

    if let Some(Value::String(text)) = fields.get("input") {
        let input = json!([{
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": text }]
        }]);
        fields.insert("input".into(), input);
    }
    require(fields, "stream", Value::Bool(true));
    require(fields, "store", Value::Bool(false));
    require(fields, "parallel_tool_calls", Value::Bool(true));
    require_include(fields);
    normalize_service_tier(fields);
    for key in UNSUPPORTED_FIELDS {
        fields.shift_remove(*key);
    }

    if let Some(Value::Array(items)) = fields.get_mut("input") {
        for item in items.iter_mut() {
            normalize_input_item(item);
        }
    }
    if let Some(Value::Array(tools)) = fields.get_mut("tools") {
        tools.iter_mut().for_each(normalize_builtin_tool);
    }
    if let Some(choice) = fields.get_mut("tool_choice") {
        normalize_builtin_tool(choice);
        if let Some(Value::Array(tools)) = choice.get_mut("tools") {
            tools.iter_mut().for_each(normalize_builtin_tool);
        }
    }
    request
}

/// Sets a field Codex requires, unless it already has that value.
fn require(fields: &mut Map<String, Value>, key: &str, value: Value) {
    if fields.get(key) != Some(&value) {
        fields.insert(key.into(), value);
    }
}

/// Sets `include` to the encrypted reasoning, followed by web search sources
/// if the client asked for them; every other entry is dropped
/// (`setCodexRequiredInclude`). An `include` already in that form is kept.
fn require_include(fields: &mut Map<String, Value>) {
    let include_sources = matches!(
        fields.get("include"),
        Some(Value::Array(items)) if items.iter().any(|item| item == WEB_SEARCH_SOURCES)
    );
    let want: &[&str] = if include_sources {
        &[ENCRYPTED_REASONING, WEB_SEARCH_SOURCES]
    } else {
        &[ENCRYPTED_REASONING]
    };
    let normalized = matches!(
        fields.get("include"),
        Some(Value::Array(items)) if items.len() == want.len()
            && items.iter().zip(want).all(|(item, want)| item == *want)
    );
    if !normalized {
        fields.insert("include".into(), json!(want));
    }
}

/// Keeps the tiers Codex offers, spelled as Codex spells them, and drops the rest.
fn normalize_service_tier(fields: &mut Map<String, Value>) {
    let Some(tier) = fields.get("service_tier") else {
        return;
    };
    let normalized = match tier {
        Value::String(tier) => match go::to_lower(tier.trim()).as_str() {
            "priority" | "fast" => Some("priority"),
            "ultrafast" => Some("ultrafast"),
            _ => None,
        },
        _ => None,
    };
    match normalized {
        Some(normalized) if tier != normalized => {
            fields.insert("service_tier".into(), normalized.into());
        }
        Some(_) => {}
        None => {
            fields.shift_remove("service_tier");
        }
    }
}

/// Fixes one `input` item: drops cache breakpoints Codex rejects, renames the
/// `system` role, which Codex doesn't accept, and gives a call with blank
/// arguments an empty object, since Codex requires valid JSON there.
fn normalize_input_item(item: &mut Value) {
    let Value::Object(item) = item else {
        return;
    };
    // Message content parts and function_call_output parts can each carry one.
    for key in ["content", "output"] {
        if let Some(Value::Array(parts)) = item.get_mut(key) {
            for part in parts.iter_mut().filter_map(Value::as_object_mut) {
                part.shift_remove(CACHE_BREAKPOINT);
            }
        }
    }
    item.shift_remove(CACHE_BREAKPOINT);

    if str_of(item.get("role")) == "system" {
        item.insert("role".into(), "developer".into());
    }
    if str_of(item.get("type")) == "function_call"
        && matches!(item.get("arguments"), Some(Value::String(arguments)) if arguments.trim().is_empty())
    {
        item.insert("arguments".into(), "{}".into());
    }
}

/// Renames preview aliases of built-in tools to the names Codex uses.
fn normalize_builtin_tool(tool: &mut Value) {
    if let Some(kind) = tool.get_mut("type")
        && matches!(
            kind.as_str(),
            Some("web_search_preview" | "web_search_preview_2025_03_11")
        )
    {
        *kind = "web_search".into();
    }
}

#[cfg(test)]
mod tests;
