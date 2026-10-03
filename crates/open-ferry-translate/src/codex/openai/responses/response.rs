// Ported from CLIProxyAPI internal/translator/codex/openai/responses/codex_openai-responses_response.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Codex events → OpenAI Responses events.
//!
//! Codex already streams Responses events, so lines pass through as they are.
//! The one change: when `response.created` or `response.in_progress` names no
//! model, we fill in the one the client asked for.
//!
//! Deviations from upstream:
//! - An event we add the model to is written as compact JSON. Upstream inserts
//!   the field into the line's original text.
//! - A `data:` line that is not valid JSON passes through unchanged. gjson
//!   reads what it can from malformed JSON, so upstream may still add a model.
//! - Not ported yet: the `apply_patch` bridge, which an executor turns on
//!   through a parameter the translator doesn't otherwise use. Upstream's
//!   Codex executor never does; its xAI, Meta and Kimi executors, which
//!   aren't ported, do.

use std::borrow::Cow;

use serde_json::{Map, Value};

use crate::common::request_model_name;
use crate::json::str_of;

/// Translates a Codex event stream into Responses events, one line at a time.
pub struct CodexToOpenAIResponsesStream {
    /// The model to report when Codex doesn't name one.
    model: Option<String>,
}

impl CodexToOpenAIResponsesStream {
    /// `original_request` is the client's request and `request` the one sent to
    /// Codex; the model is read from them, or else is `model`.
    pub fn new(model: &str, original_request: &Value, request: &Value) -> Self {
        let model = request_model_name(original_request, request).unwrap_or(model);
        Self {
            model: (!model.is_empty()).then(|| model.to_owned()),
        }
    }

    /// Translates one line of the Codex event stream into one line for the
    /// client. Lines that need no change are returned as they are.
    pub fn translate_line<'l>(&self, line: &'l [u8]) -> Cow<'l, [u8]> {
        let Some(model) = &self.model else {
            return Cow::Borrowed(line);
        };
        let (prefix, data) = match line.strip_prefix(b"data:") {
            Some(data) => (&b"data: "[..], data),
            None => (&b""[..], line),
        };
        let event = std::str::from_utf8(data)
            .ok()
            .and_then(|data| serde_json::from_str(data.trim()).ok());
        let Some(mut event) = event else {
            return Cow::Borrowed(line);
        };
        if !set_model(&mut event, model) {
            return Cow::Borrowed(line);
        }
        let mut out = prefix.to_vec();
        serde_json::to_writer(&mut out, &event).expect("a JSON value always serializes");
        Cow::Owned(out)
    }
}

/// Names the model in a `response.created` or `response.in_progress` event
/// that has none. Returns whether the event changed.
fn set_model(event: &mut Value, model: &str) -> bool {
    let kind = str_of(event.get("type"));
    if kind != "response.created" && kind != "response.in_progress" {
        return false;
    }
    let Value::Object(fields) = event else {
        return false;
    };
    let response = fields
        .entry("response")
        .or_insert_with(|| Value::Object(Map::new()));
    match response {
        Value::Object(response) if response.contains_key("model") => false,
        Value::Object(response) => {
            response.insert("model".into(), model.into());
            true
        }
        // sjson can't set a key in an array.
        Value::Array(_) => false,
        // It replaces any other value with an object.
        other => {
            *other = Value::Object(Map::from_iter([("model".into(), model.into())]));
            true
        }
    }
}

/// Converts Codex's final event into the Responses API's non-streaming body:
/// the `response` of a `response.completed` or `response.incomplete` event.
/// A body that is already a response (no `type`, with an `output` list) is
/// returned whole. Returns `None` for any other event.
pub fn convert_codex_response_to_openai_responses_non_stream(mut event: Value) -> Option<Value> {
    let kind = str_of(event.get("type"));
    let is_response = kind.is_empty() && event.get("output").is_some_and(Value::is_array);
    let is_final = kind == "response.completed" || kind == "response.incomplete";
    if is_response {
        Some(event)
    } else if is_final {
        event.get_mut("response").map(Value::take)
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
