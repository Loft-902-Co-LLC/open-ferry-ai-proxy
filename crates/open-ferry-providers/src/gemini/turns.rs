// Ported from CLIProxyAPI internal/runtime/executor/helps/gemini_content_turns.go
// (EnsureGeminiLeadingUserContent, EnsureGeminiTrailingUserContent,
// EnsureGeminiBoundaryUserContent, DropGeminiTrailingThoughtOnlyContent) and helps/vertex_payload_helpers.go
// (StripVertexOpenAIResponsesToolCallIDs) (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The turns of a Gemini request's `contents`, as Gemini and Vertex AI take
//! them.
//!
//! Gemini rejects a conversation that starts with the model, and answers a
//! conversation that ends with the model as if the user had said nothing,
//! so an empty user turn goes before a first model turn and, for a request
//! that generates content, after a last one, unless that last turn answers
//! a function call without nested media. Vertex AI also rejects the call
//! IDs of an OpenAI Responses client's function calls and results. Vertex drops
//! trailing thought-only turns before adding either boundary in streams; other
//! generating calls add the leading user turn first, as upstream does.
//!
//! Contents are edited as JSON arrays in place rather than rebuilt as raw JSON.

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
/// unless that turn has a function response without nested media.
pub(crate) fn ensure_trailing_user(body: &mut Value, path: &str) {
    let Some(Value::Array(contents)) = json::get_mut(body, path) else {
        return;
    };
    let Some(last) = contents.last() else {
        return;
    };
    let role = json::str_of(last.get("role"));
    if (role != "model" && role != "assistant")
        || (has_function_response(last) && !has_function_response_media(last))
    {
        return;
    }
    contents.push(empty_user_turn());
}

/// `DropGeminiTrailingThoughtOnlyContent`: removes trailing reasoning turns,
/// retaining at least one turn. Run before adding the synthetic user turn.
pub(crate) fn drop_trailing_thought_only(body: &mut Value, path: &str) {
    let Some(Value::Array(contents)) = json::get_mut(body, path) else {
        return;
    };
    while contents.len() > 1 && contents.last().is_some_and(thought_only_model_turn) {
        contents.pop();
    }
}

fn thought_only_model_turn(content: &Value) -> bool {
    matches!(
        json::str_of(content.get("role")).as_str(),
        "model" | "assistant"
    ) && content
        .get("parts")
        .filter(|parts| !parts.is_null())
        .is_some_and(|parts| {
            let parts = parts
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_else(|| std::slice::from_ref(parts));
            !parts.is_empty()
                && parts.iter().all(|part| {
                    json::bool_of(part.get("thought"))
                        && part.get("functionCall").is_none()
                        && part.get("functionResponse").is_none()
                })
        })
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

/// `contentHasFunctionResponseMedia`: nested media parts require a user
/// turn after a model-role function response.
fn has_function_response_media(content: &Value) -> bool {
    let Some(Value::Array(parts)) = content.get("parts") else {
        return false;
    };
    parts.iter().any(|part| {
        let has_media = |part: &Value| {
            ["inlineData", "inline_data", "fileData", "file_data"]
                .iter()
                .any(|key| part.get(key).is_some())
        };
        match json::get(part, "functionResponse.parts") {
            Some(Value::Array(nested)) => nested.iter().any(has_media),
            Some(nested) => has_media(nested),
            None => false,
        }
    })
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
