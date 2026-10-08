// Ported from CLIProxyAPI internal/translator/openai/openai/chat-completions/openai_openai_request.go
// (ConvertOpenAIRequestToOpenAI) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Chat Completions request → Chat Completions request.
//!
//! Deviations from upstream: none.

use serde_json::Value;

use crate::json::set_path;

/// Passes a Chat Completions request on, with `model` set to `model_name`.
/// A request that already names that model as a string is returned as it
/// is. One whose top level is an array can't take the field, so it is
/// returned as it is too.
pub fn convert_openai_request_to_openai(model_name: &str, mut request: Value) -> Value {
    if request.get("model").and_then(Value::as_str) == Some(model_name) {
        return request;
    }
    set_path(&mut request, "model", Value::String(model_name.to_owned()));
    request
}

#[cfg(test)]
mod tests;
