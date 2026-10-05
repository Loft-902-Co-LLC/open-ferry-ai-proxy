// Ported from CLIProxyAPI internal/runtime/executor/helps/gemini_content_turns.go
// (EnsureGeminiLeadingUserContent, EnsureGeminiTrailingUserContent,
// EnsureGeminiBoundaryUserContent) and helps/vertex_payload_helpers.go
// (StripVertexOpenAIResponsesToolCallIDs) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The turns of a Gemini request's `contents`, as Gemini and Vertex AI take
//! them.
//!
//! Gemini rejects a conversation that starts with the model, and answers a
//! conversation that ends with the model as if the user had said nothing,
//! so an empty user turn goes before a first model turn and, for a request
//! that generates content, after a last one, unless that last turn answers
//! a function call. Vertex AI also rejects the call IDs of an OpenAI
//! Responses client's function calls and results.
//!
//! Deviations from upstream: none.

use serde_json::{Value, json};

use crate::json;

/// An empty user turn.
fn empty_user_turn() -> Value {
    json!({"role": "user", "parts": [{"text": ""}]})
}

/// `EnsureGeminiLeadingUserContent`: puts an empty user turn before a first
/// turn of the model's in the array at `path`.
pub(crate) fn ensure_leading_user(body: &mut Value, path: &str) {
    if json::str_at(body, &format!("{path}.0.role")) != "model" {
        return;
    }
    if let Some(Value::Array(contents)) = json::get_mut(body, path)
        && !contents.is_empty()
    {
        contents.insert(0, empty_user_turn());
    }
}

/// `EnsureGeminiTrailingUserContent`: puts an empty user turn after a last
/// turn of the model's (`model` or `assistant`) in the array at `path`,
/// unless that turn has a function response.
pub(crate) fn ensure_trailing_user(body: &mut Value, path: &str) {
    let Some(Value::Array(contents)) = json::get_mut(body, path) else {
        return;
    };
    let Some(last) = contents.last() else {
        return;
    };
    let role = json::str_of(last.get("role"));
    if (role != "model" && role != "assistant") || has_function_response(last) {
        return;
    }
    contents.push(empty_user_turn());
}

/// `EnsureGeminiBoundaryUserContent`: both of the above.
pub(crate) fn ensure_boundary_user(body: &mut Value, path: &str) {
    ensure_leading_user(body, path);
    ensure_trailing_user(body, path);
}

/// `contentHasFunctionResponse`.
fn has_function_response(content: &Value) -> bool {
    match content.get("parts") {
        Some(Value::Array(parts)) => parts
            .iter()
            .any(|part| part.get("functionResponse").is_some()),
        _ => false,
    }
}

/// `StripVertexOpenAIResponsesToolCallIDs`: for a request from an OpenAI
/// Responses client (`from`), removes `functionCall.id` and
/// `functionResponse.id` from the parts of each turn in `contents`.
pub(crate) fn strip_vertex_tool_call_ids(body: &mut Value, from: &str) {
    if !json::eq_fold(from.trim(), "openai-response") {
        return;
    }
    let Some(Value::Array(contents)) = body.get_mut("contents") else {
        return;
    };
    for content in contents {
        let Some(Value::Array(parts)) = content.get_mut("parts") else {
            continue;
        };
        for part in parts {
            json::delete(part, "functionCall.id");
            json::delete(part, "functionResponse.id");
        }
    }
}

#[cfg(test)]
mod tests;
