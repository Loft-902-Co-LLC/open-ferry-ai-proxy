// Ported from CLIProxyAPI internal/translator/codex/gemini/codex_gemini_response.go (v8.0.32, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Codex (OpenAI Responses) events → Gemini `generateContent` responses.
//!
//! Deviations from upstream:
//! - `createTime` is written in UTC, where upstream uses the server's local
//!   time zone.
//! - Function call arguments that start like a JSON object but aren't valid
//!   JSON are left out, so `args` stays `{}`. Upstream copies them in as they
//!   are and writes invalid JSON.
//! - A non-string value read as text, such as a reasoning item's `content`
//!   array, is written as compact JSON, where upstream uses its JSON text.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::request::{build_short_name_map, declared_names};
use crate::common::gemini_response::{create_time, gemini_token_count_json};
use crate::go;
use crate::json::{exact, int_of, object, path, str_of};

/// The `createTime` upstream's stream template starts with, kept when an
/// event has no `response.created_at`.
const TEMPLATE_CREATE_TIME: &str = "2025-08-15T02:52:03.884209Z";

/// Converts a token count into a Gemini `countTokens` response body.
pub fn gemini_token_count(count: i64) -> Value {
    gemini_token_count_json(count)
}

/// Translates a Codex event stream into Gemini response chunks, one `data:`
/// line at a time. Keep one per response.
pub struct CodexToGeminiStream {
    model: String,
    response_id: String,
    /// A function call, held back until the next chunk goes out.
    pending_function_calls: Vec<Value>,
    pending_signature: String,
    seen_reasoning: HashSet<[u8; 32]>,
    has_output_text_delta: bool,
    /// The SHA-256 of the last image sent for each image item, so the same
    /// image isn't sent twice.
    last_image_hash_by_id: HashMap<String, [u8; 32]>,
    /// Codex tool name → the name the client declared.
    tool_names: HashMap<String, String>,
}

impl CodexToGeminiStream {
    /// Starts a stream for a client that asked for `model` with
    /// `original_request`.
    pub fn new(model: &str, original_request: &Value) -> Self {
        Self {
            model: model.to_owned(),
            response_id: String::new(),
            pending_function_calls: Vec::new(),
            pending_signature: String::new(),
            seen_reasoning: HashSet::new(),
            has_output_text_delta: false,
            last_image_hash_by_id: HashMap::new(),
            tool_names: reverse_tool_names(original_request),
        }
    }

    /// Translates one line of the Codex stream into zero or more Gemini
    /// response chunks. Lines other than `data:` lines give nothing.
    pub fn translate_line(&mut self, line: &[u8]) -> Vec<Value> {
        let Some(payload) = line.strip_prefix(b"data:") else {
            return Vec::new();
        };
        let event: Value = serde_json::from_slice(go::trim_space(payload)).unwrap_or(Value::Null);
        let kind = str_of(event.get("type"));

        let create_time = match path(&event, "response.created_at") {
            Some(created_at) => create_time(int_of(created_at)),
            None => TEMPLATE_CREATE_TIME.to_owned(),
        };
        let mut template = object([
            (
                "candidates",
                Value::Array(vec![object([(
                    "content",
                    object([
                        ("role", "model".into()),
                        ("parts", Value::Array(Vec::new())),
                    ]),
                )])]),
            ),
            (
                "usageMetadata",
                object([("trafficType", "PROVISIONED_THROUGHPUT".into())]),
            ),
            ("modelVersion", self.model.clone().into()),
            ("createTime", create_time.into()),
            ("responseId", self.response_id.clone().into()),
        ]);

        match kind.as_ref() {
            "response.image_generation_call.partial_image" => {
                let item_id = str_of(event.get("item_id"));
                let data = str_of(event.get("partial_image_b64"));
                if data.is_empty() || self.repeats_image(&item_id, &data) {
                    return Vec::new();
                }
                let mime_type = image_mime_type(&str_of(event.get("output_format")));
                let mut part = inline_data(&data, &mime_type);
                self.attach_signature(&mut part);
                *parts_mut(&mut template) = vec![part];
                return self.flush(template);
            }
            "response.output_item.done" => {
                let item = event.get("item").unwrap_or(&Value::Null);
                match str_of(item.get("type")).as_ref() {
                    "image_generation_call" => {
                        let item_id = str_of(item.get("id"));
                        let data = str_of(item.get("result"));
                        if data.is_empty() || self.repeats_image(&item_id, &data) {
                            return Vec::new();
                        }
                        let mime_type = image_mime_type(&str_of(item.get("output_format")));
                        let mut part = inline_data(&data, &mime_type);
                        self.attach_signature(&mut part);
                        *parts_mut(&mut template) = vec![part];
                        return self.flush(template);
                    }
                    "reasoning" => {
                        let sig = str_of(item.get("encrypted_content")).trim().to_owned();
                        if sig.is_empty() { return Vec::new(); }
                        let key: [u8; 32] = Sha256::digest(format!("{}\0{sig}", str_of(item.get("id"))).as_bytes()).into();
                        if !self.seen_reasoning.insert(key) { return Vec::new(); }
                        let previous = std::mem::replace(&mut self.pending_signature, sig);
                        if previous.is_empty() { return Vec::new(); }
                        let mut part = serde_json::json!({"thought":true,"text":""});
                        super::signature::set(&mut part, &previous);
                        *parts_mut(&mut template) = vec![part];
                        return self.flush(template);
                    }
                    "function_call" => {
                        let mut call = function_call(item, &self.tool_names, CallKeyOrder::NameFirst);
                        self.attach_signature(&mut call);
                        *parts_mut(&mut template) = vec![call];
                        set_finish_reason(&mut template, "STOP");
                        self.pending_function_calls.push(template);
                        return Vec::new();
                    }
                    "message" if !self.has_output_text_delta => {}
                    _ => return Vec::new(),
                }
                // No text deltas came, so send the finished message's text.
                let Some(Value::Array(content)) = item.get("content") else {
                    return Vec::new();
                };
                let mut texts: Vec<Value> = content
                    .iter()
                    .filter(|part| str_of(part.get("type")) == "output_text")
                    .map(|part| str_of(part.get("text")))
                    .filter(|text| !text.is_empty())
                    .map(|text| object([("text", text.into_owned().into())]))
                    .collect();
                if texts.is_empty() {
                    return Vec::new();
                }
                self.has_output_text_delta = true;
                self.attach_signature(&mut texts[0]);
                *parts_mut(&mut template) = texts;
                return self.flush(template);
            }
            "response.created" => {
                self.pending_function_calls.clear();
                self.pending_signature.clear();
                self.seen_reasoning.clear();
                self.has_output_text_delta = false;
                let model = str_of(path(&event, "response.model")).into_owned();
                let id = str_of(path(&event, "response.id")).into_owned();
                template["modelVersion"] = model.into();
                template["responseId"] = id.clone().into();
                self.response_id = id;
            }
            "response.reasoning_summary_text.delta" => {
                let text = str_of(event.get("delta")).into_owned();
                *parts_mut(&mut template) =
                    vec![object([("thought", true.into()), ("text", text.into())])];
            }
            "response.output_text.delta" => {
                self.has_output_text_delta = true;
                let text = str_of(event.get("delta")).into_owned();
                let mut part = object([("text", text.clone().into())]);
                if !text.is_empty() { self.attach_signature(&mut part); }
                *parts_mut(&mut template) = vec![part];
            }
            "response.completed" | "response.incomplete" => {
                if !self.pending_signature.is_empty() {
                    let mut part = serde_json::json!({"thought":true,"text":""});
                    self.attach_signature(&mut part);
                    *parts_mut(&mut template) = vec![part];
                }
                let response = event.get("response").unwrap_or(&Value::Null);
                set_usage(&mut template, response);
                if kind == "response.incomplete" {
                    let reason = str_of(path(response, "incomplete_details.reason"));
                    set_finish_reason(&mut template, incomplete_finish_reason(&reason));
                }
            }
            _ => return Vec::new(),
        }

        self.flush(template)
    }

    fn attach_signature(&mut self, part: &mut Value) {
        super::signature::set(part, &self.pending_signature);
        self.pending_signature.clear();
    }

    fn flush(&mut self, template: Value) -> Vec<Value> {
        let mut out = std::mem::take(&mut self.pending_function_calls);
        out.push(template);
        out
    }

    /// Whether `data` is the image last sent for `item_id`, recording it if
    /// not. Images without an item ID are always sent.
    fn repeats_image(&mut self, item_id: &str, data: &str) -> bool {
        if item_id.is_empty() {
            return false;
        }
        let hash: [u8; 32] = Sha256::digest(data.as_bytes()).into();
        if self.last_image_hash_by_id.get(item_id) == Some(&hash) {
            return true;
        }
        self.last_image_hash_by_id.insert(item_id.to_owned(), hash);
        false
    }
}

/// Converts a Codex `response.completed` or `response.incomplete` event into
/// a whole Gemini response, or `None` (upstream's empty body) for any other
/// event.
pub fn convert_codex_response_to_gemini_non_stream(
    model: &str,
    original_request: &Value,
    event: &Value,
) -> Option<Value> {
    let kind = str_of(event.get("type"));
    if kind != "response.completed" && kind != "response.incomplete" {
        return None;
    }

    let mut out = object([
        (
            "candidates",
            Value::Array(vec![object([
                (
                    "content",
                    object([
                        ("role", "model".into()),
                        ("parts", Value::Array(Vec::new())),
                    ]),
                ),
                ("finishReason", "STOP".into()),
            ])]),
        ),
        (
            "usageMetadata",
            object([("trafficType", "PROVISIONED_THROUGHPUT".into())]),
        ),
        ("modelVersion", model.into()),
        ("createTime", "".into()),
        ("responseId", "".into()),
    ]);

    let Some(response) = event.get("response") else {
        return Some(out);
    };
    if kind == "response.incomplete" {
        let reason = str_of(path(response, "incomplete_details.reason"));
        set_finish_reason(&mut out, incomplete_finish_reason(&reason));
    }
    if let Some(id) = response.get("id") {
        out["responseId"] = str_of(Some(id)).into_owned().into();
    }
    if let Some(created_at) = response.get("created_at") {
        out["createTime"] = create_time(int_of(created_at)).into();
    }
    if response.get("usage").is_some() {
        set_usage(&mut out, response);
    }

    let Some(Value::Array(output)) = response.get("output") else {
        return Some(out);
    };
    let tool_names = reverse_tool_names(original_request);
    let mut parts = Vec::new();
    for item in output {
        match str_of(item.get("type")).as_ref() {
            "reasoning" => {
                let sig = str_of(item.get("encrypted_content")).trim().to_owned();
                let mut texts = Vec::new();
                if let Some(summary) = item.get("summary").and_then(Value::as_array) {
                    texts.extend(summary.iter().map(|p| str_of(p.get("text")).into_owned()).filter(|t| !t.is_empty()));
                }
                match item.get("content") {
                    Some(Value::Array(content)) => texts.extend(content.iter().filter(|p| str_of(p.get("type")) == "reasoning_text").map(|p| str_of(p.get("text")).into_owned()).filter(|t| !t.is_empty())),
                    Some(Value::String(text)) if !text.is_empty() => texts.push(text.clone()),
                    _ => {}
                }
                if texts.is_empty() && !sig.is_empty() { texts.push(String::new()); }
                for (i, text) in texts.into_iter().enumerate() {
                    let mut part = object([("text", text.into()), ("thought", true.into())]);
                    if i == 0 { super::signature::set(&mut part, &sig); }
                    parts.push(part);
                }
            }
            "message" => {
                if let Some(Value::Array(content)) = item.get("content") {
                    for part in content {
                        if str_of(part.get("type")) != "output_text" {
                            continue;
                        }
                        if let Some(text) = part.get("text") {
                            let text = str_of(Some(text)).into_owned();
                            parts.push(object([("text", text.into())]));
                        }
                    }
                }
            }
            "image_generation_call" => {
                let data = str_of(item.get("result"));
                if !data.is_empty() {
                    let mime_type = image_mime_type(&str_of(item.get("output_format")));
                    parts.push(inline_data(&data, &mime_type));
                }
            }
            "function_call" => {
                parts.push(function_call(item, &tool_names, CallKeyOrder::ArgsFirst));
            }
            _ => {}
        }
    }
    if !parts.is_empty() {
        *parts_mut(&mut out) = parts;
    }
    Some(out)
}

/// Codex tool name → the name the client declared, from the client's
/// request (`buildReverseMapFromGeminiOriginal`).
fn reverse_tool_names(original_request: &Value) -> HashMap<String, String> {
    build_short_name_map(&declared_names(original_request))
        .into_iter()
        .map(|(name, short)| (short, name))
        .collect()
}

/// The order upstream's two `functionCall` templates write their keys in.
#[derive(Clone, Copy)]
enum CallKeyOrder {
    NameFirst,
    ArgsFirst,
}

/// A Codex `function_call` item as a Gemini `functionCall` part.
fn function_call(item: &Value, tool_names: &HashMap<String, String>, order: CallKeyOrder) -> Value {
    let name = str_of(item.get("name"));
    let name = tool_names
        .get(name.as_ref())
        .cloned()
        .unwrap_or_else(|| name.into_owned());
    let args =
        arguments_object(&str_of(item.get("arguments"))).unwrap_or_else(|| Map::new().into());
    let mut call = Map::new();
    match order {
        CallKeyOrder::NameFirst => {
            call.insert("name".into(), name.into());
            call.insert("args".into(), args);
        }
        CallKeyOrder::ArgsFirst => {
            call.insert("args".into(), args);
            call.insert("name".into(), name.into());
        }
    }
    // `setGeminiFunctionCallID`: the trimmed `call_id`, or else the trimmed `id`.
    if let Some(id) = ["call_id", "id"].into_iter().find_map(|key| {
        let id = str_of(item.get(key));
        let id = id.trim();
        (!id.is_empty()).then(|| id.to_owned())
    }) {
        call.insert("id".into(), id.into());
    }
    object([("functionCall", Value::Object(call))])
}

/// Function call arguments, if they are a JSON object, each number as
/// written.
fn arguments_object(arguments: &str) -> Option<Value> {
    match exact::from_str(arguments) {
        Ok(args @ Value::Object(_)) => Some(args),
        _ => None,
    }
}

fn inline_data(data: &str, mime_type: &str) -> Value {
    object([(
        "inlineData",
        object([("data", data.into()), ("mimeType", mime_type.into())]),
    )])
}

/// `mimeTypeFromCodexOutputFormat`.
fn image_mime_type(output_format: &str) -> String {
    if output_format.is_empty() {
        return "image/png".to_owned();
    }
    if output_format.contains('/') {
        return output_format.to_owned();
    }
    match go::to_lower(output_format).as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "image/png",
    }
    .to_owned()
}

fn incomplete_finish_reason(reason: &str) -> &'static str {
    match reason {
        "max_tokens" | "max_output_tokens" => "MAX_TOKENS",
        "content_filter" => "SAFETY",
        _ => "OTHER",
    }
}

/// The first candidate's parts.
fn parts_mut(template: &mut Value) -> &mut Vec<Value> {
    match &mut template["candidates"][0]["content"]["parts"] {
        Value::Array(parts) => parts,
        _ => unreachable!("the template has a parts list"),
    }
}

fn set_finish_reason(template: &mut Value, reason: &str) {
    template["candidates"][0]["finishReason"] = reason.into();
}

/// Copies the response's token counts. Go adds them as `int64`, wrapping on
/// overflow.
fn set_usage(template: &mut Value, response: &Value) {
    let input = path(response, "usage.input_tokens").map_or(0, int_of);
    let output = path(response, "usage.output_tokens").map_or(0, int_of);
    let usage = &mut template["usageMetadata"];
    usage["promptTokenCount"] = input.into();
    usage["candidatesTokenCount"] = output.into();
    usage["totalTokenCount"] = input.wrapping_add(output).into();
}

#[cfg(test)]
mod tests;
