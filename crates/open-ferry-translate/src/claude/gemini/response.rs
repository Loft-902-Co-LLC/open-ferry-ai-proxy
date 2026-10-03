// Ported from CLIProxyAPI internal/translator/claude/gemini/claude_gemini_response.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Claude Messages events → Gemini `generateContent` responses.
//!
//! Deviations from upstream:
//! - `createTime` is written in UTC, where upstream uses the server's local
//!   time zone.
//! - Tool input that isn't valid JSON once its pieces are joined is left out,
//!   so `args` stays `{}`. Upstream copies it in as it is and writes invalid
//!   JSON.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use crate::common::gemini_response::{create_time, gemini_token_count_json};
use crate::go;
use crate::json::{int_of, object, path, str_of};
use crate::signature::{BlockKind, gemini_replay_signature_or_bypass};

/// Converts a token count into a Gemini `countTokens` response body.
pub fn gemini_token_count(count: i64) -> Value {
    gemini_token_count_json(count)
}

/// Translates a Claude Messages SSE stream into Gemini response chunks, one
/// line at a time. Keep one per response: it tracks the tool calls in
/// progress.
pub struct ClaudeToGeminiStream {
    model: String,
    /// When the first event came, in Unix seconds; 0 before that.
    created_at: i64,
    response_id: String,
    tools: ToolCalls,
}

impl ClaudeToGeminiStream {
    /// Starts a stream for a client that asked for `model`.
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            created_at: 0,
            response_id: String::new(),
            tools: ToolCalls::default(),
        }
    }

    /// Translates one SSE line into zero or more Gemini response chunks.
    /// Lines other than `data:` lines give nothing.
    pub fn translate_line(&mut self, line: &[u8]) -> Vec<Value> {
        let Some(payload) = line.strip_prefix(b"data:") else {
            return Vec::new();
        };
        let event: Value = serde_json::from_slice(go::trim_space(payload)).unwrap_or(Value::Null);

        if self.created_at == 0 {
            self.created_at = now();
        }
        let mut template = response_template(None);
        template["modelVersion"] = self.model.clone().into();
        template["createTime"] = create_time(self.created_at).into();
        template["responseId"] = self.response_id.clone().into();

        match str_of(event.get("type")).as_ref() {
            "message_start" => {
                if let Some(message) = event.get("message") {
                    self.response_id = str_of(message.get("id")).into_owned();
                    self.model = str_of(message.get("model")).into_owned();
                }
                Vec::new()
            }
            "content_block_start" => match self.tools.start(&event) {
                Some(part) => {
                    parts_mut(&mut template).push(part);
                    vec![template]
                }
                None => Vec::new(),
            },
            "content_block_delta" => match event.get("delta") {
                Some(delta) if str_of(delta.get("type")) == "input_json_delta" => {
                    self.tools.add_input(&event, delta);
                    Vec::new()
                }
                Some(delta) => {
                    parts_mut(&mut template).extend(delta_part(delta));
                    vec![template]
                }
                None => vec![template],
            },
            "content_block_stop" => match self.tools.stop(&event) {
                Some(call) => {
                    parts_mut(&mut template).push(call);
                    template["candidates"][0]["finishReason"] = "STOP".into();
                    vec![template]
                }
                None => Vec::new(),
            },
            "message_delta" => {
                if let Some(usage) = event.get("usage") {
                    let usage_metadata = &mut template["usageMetadata"];
                    for (key, value) in usage_fields(usage) {
                        usage_metadata[key] = value;
                    }
                }
                // Upstream sets the stop reason, then always sets `STOP` over it.
                template["candidates"][0]["finishReason"] = "STOP".into();
                vec![template]
            }
            "error" => {
                let mut message = str_of(path(&event, "error.message")).into_owned();
                if message.is_empty() {
                    message = "Unknown error occurred".to_owned();
                }
                vec![object([(
                    "error",
                    object([
                        ("code", 400.into()),
                        ("message", message.into()),
                        ("status", "INVALID_ARGUMENT".into()),
                    ]),
                )])]
            }
            _ => Vec::new(),
        }
    }
}

/// Converts a whole Claude Messages response, given as its SSE event stream,
/// into a Gemini response.
pub fn convert_claude_response_to_gemini_non_stream(model: &str, body: &[u8]) -> Value {
    let mut out = response_template(Some("STOP"));
    out["modelVersion"] = model.into();

    let mut tools = ToolCalls::default();
    let mut parts = Vec::new();
    let mut usage_metadata = None;
    let mut response_id = String::new();
    let mut created_at = 0;

    for line in body.split(|&b| b == b'\n') {
        let line = trim_right_cr(line);
        let Some(payload) = line.strip_prefix(b"data:") else {
            continue;
        };
        let payload = go::trim_space(payload);
        if payload.is_empty() {
            continue;
        }
        let event: Value = serde_json::from_slice(payload).unwrap_or(Value::Null);
        match str_of(event.get("type")).as_ref() {
            "message_start" => {
                if let Some(message) = event.get("message") {
                    response_id = str_of(message.get("id")).into_owned();
                    created_at = now();
                }
            }
            "content_block_start" => parts.extend(tools.start(&event)),
            "content_block_delta" => match event.get("delta") {
                Some(delta) if str_of(delta.get("type")) == "input_json_delta" => {
                    tools.add_input(&event, delta);
                }
                Some(delta) => parts.extend(delta_part(delta)),
                None => {}
            },
            "content_block_stop" => parts.extend(tools.stop(&event)),
            "message_delta" => {
                if let Some(usage) = event.get("usage") {
                    let mut fields: Map<String, Value> = usage_fields(usage).collect();
                    fields.insert("trafficType".into(), "PROVISIONED_THROUGHPUT".into());
                    usage_metadata = Some(fields);
                }
            }
            _ => {}
        }
    }

    if !response_id.is_empty() {
        out["responseId"] = response_id.into();
    }
    if created_at > 0 {
        out["createTime"] = create_time(created_at).into();
    }
    let parts = consolidate_parts(parts);
    if !parts.is_empty() {
        *parts_mut(&mut out) = parts;
    }
    if let Some(usage_metadata) = usage_metadata {
        out["usageMetadata"] = usage_metadata.into();
    }
    out
}

/// `bytes.TrimRight(line, "\r")`: every trailing carriage return.
fn trim_right_cr(mut line: &[u8]) -> &[u8] {
    while let Some(rest) = line.strip_suffix(b"\r") {
        line = rest;
    }
    line
}

/// The current Unix time in seconds.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Upstream's response template, with a finish reason for whole responses.
fn response_template(finish_reason: Option<&str>) -> Value {
    let mut candidate = Map::new();
    candidate.insert(
        "content".into(),
        object([
            ("role", "model".into()),
            ("parts", Value::Array(Vec::new())),
        ]),
    );
    if let Some(reason) = finish_reason {
        candidate.insert("finishReason".into(), reason.into());
    }
    object([
        ("candidates", Value::Array(vec![candidate.into()])),
        (
            "usageMetadata",
            object([("trafficType", "PROVISIONED_THROUGHPUT".into())]),
        ),
        ("modelVersion", "".into()),
        ("createTime", "".into()),
        ("responseId", "".into()),
    ])
}

/// The first candidate's parts.
fn parts_mut(template: &mut Value) -> &mut Vec<Value> {
    match &mut template["candidates"][0]["content"]["parts"] {
        Value::Array(parts) => parts,
        _ => unreachable!("the template has a parts list"),
    }
}

/// The part for a text, thinking or signature delta, if it has any content.
fn delta_part(delta: &Value) -> Option<Value> {
    let non_empty = |key: &str| {
        let text = str_of(delta.get(key)).into_owned();
        (!text.is_empty()).then_some(text)
    };
    match str_of(delta.get("type")).as_ref() {
        "text_delta" => non_empty("text").map(|text| object([("text", text.into())])),
        "thinking_delta" => non_empty("thinking")
            .map(|text| object([("thought", true.into()), ("text", text.into())])),
        "signature_delta" => non_empty("signature").map(|signature| signature_part(&signature)),
        _ => None,
    }
}

/// A thinking signature as a Gemini thought part, in the form Gemini takes it
/// back.
fn signature_part(signature: &str) -> Value {
    let signature = gemini_replay_signature_or_bypass(signature, BlockKind::GeminiModelPart);
    object([
        ("thought", true.into()),
        ("thoughtSignature", signature.into()),
    ])
}

/// Gemini's token counts for a `message_delta`'s usage, in upstream's order.
/// Go adds the counts as `int64`, wrapping on overflow.
fn usage_fields(usage: &Value) -> impl Iterator<Item = (String, Value)> {
    let count = |key: &str| usage.get(key).map_or(0, int_of);
    let input = count("input_tokens");
    let output = count("output_tokens");
    let mut fields = vec![
        ("promptTokenCount", Value::from(input)),
        ("candidatesTokenCount", output.into()),
        ("totalTokenCount", input.wrapping_add(output).into()),
    ];
    let creation = usage.get("cache_creation_input_tokens");
    let read = usage.get("cache_read_input_tokens");
    if let Some(read) = read {
        let total = count("cache_creation_input_tokens").wrapping_add(int_of(read));
        fields.push(("cachedContentTokenCount", total.into()));
    } else if let Some(creation) = creation {
        fields.push(("cachedContentTokenCount", int_of(creation).into()));
    }
    if let Some(thinking) = usage.get("thinking_tokens") {
        fields.push(("thoughtsTokenCount", int_of(thinking).into()));
    }
    fields
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
}

/// The tool calls being streamed, by content block index.
#[derive(Default)]
struct ToolCalls {
    names: HashMap<i64, String>,
    inputs: HashMap<i64, String>,
    ids: HashMap<i64, String>,
}

impl ToolCalls {
    /// A `content_block_start`: records a tool call's name and ID, or gives
    /// the part for a thinking block that starts with a signature.
    fn start(&mut self, event: &Value) -> Option<Value> {
        let block = event.get("content_block")?;
        match str_of(block.get("type")).as_ref() {
            "tool_use" => {
                let index = event.get("index").map_or(0, int_of);
                if let Some(name) = block.get("name") {
                    self.names.insert(index, str_of(Some(name)).into_owned());
                }
                let id = str_of(block.get("id"));
                if !id.is_empty() {
                    self.ids.insert(index, id.into_owned());
                }
                None
            }
            "thinking" => {
                let signature = str_of(block.get("signature"));
                (!signature.is_empty()).then(|| signature_part(&signature))
            }
            _ => None,
        }
    }

    /// An `input_json_delta`: one more piece of a tool call's input.
    fn add_input(&mut self, event: &Value, delta: &Value) {
        let index = event.get("index").map_or(0, int_of);
        let input = self.inputs.entry(index).or_default();
        if let Some(piece) = delta.get("partial_json") {
            input.push_str(&str_of(Some(piece)));
        }
    }

    /// A `content_block_stop`: the finished function call, if the block was a
    /// tool call with a name or some input.
    fn stop(&mut self, event: &Value) -> Option<Value> {
        let index = event.get("index").map_or(0, int_of);
        let name = self.names.get(&index).cloned().unwrap_or_default();
        let input = self
            .inputs
            .get(&index)
            .map(|input| input.trim().to_owned())
            .unwrap_or_default();
        if name.is_empty() && input.is_empty() {
            return None;
        }
        let id = self.ids.get(&index).cloned().unwrap_or_default();
        self.names.remove(&index);
        self.inputs.remove(&index);
        self.ids.remove(&index);

        let args = if input.is_empty() {
            None
        } else {
            serde_json::from_str(&input).ok()
        };
        let mut call = Map::new();
        call.insert("name".into(), name.into());
        call.insert(
            "args".into(),
            args.unwrap_or_else(|| Value::Object(Map::new())),
        );
        if !id.is_empty() {
            call.insert("id".into(), id.into());
        }
        Some(object([("functionCall", call.into())]))
    }
}

/// `consolidateParts`: joins runs of text parts into one, and runs of thought
/// parts into one that keeps the last signature. Other parts stay as they
/// are, in order.
fn consolidate_parts(parts: Vec<Value>) -> Vec<Value> {
    let mut consolidated = Vec::new();
    let mut text = String::new();
    let mut thought = String::new();
    let mut signature = String::new();
    let mut has_thought = false;

    fn flush_text(text: &mut String, consolidated: &mut Vec<Value>) {
        if !text.is_empty() {
            consolidated.push(object([("text", std::mem::take(text).into())]));
        }
    }
    fn flush_thought(
        thought: &mut String,
        signature: &mut String,
        has_thought: &mut bool,
        consolidated: &mut Vec<Value>,
    ) {
        if *has_thought && (!thought.is_empty() || !signature.is_empty()) {
            let mut part = Map::new();
            part.insert("thought".into(), true.into());
            part.insert("text".into(), std::mem::take(thought).into());
            if !signature.is_empty() {
                part.insert("thoughtSignature".into(), std::mem::take(signature).into());
            }
            consolidated.push(part.into());
            *has_thought = false;
        }
    }

    for part in parts {
        if part.get("thought") == Some(&Value::Bool(true)) {
            flush_text(&mut text, &mut consolidated);
            if let Some(Value::String(piece)) = part.get("text") {
                thought.push_str(piece);
                has_thought = true;
            }
            if let Some(Value::String(piece)) = part.get("thoughtSignature")
                && !piece.is_empty()
            {
                signature.clone_from(piece);
                has_thought = true;
            }
        } else if let Some(Value::String(piece)) = part.get("text") {
            flush_thought(
                &mut thought,
                &mut signature,
                &mut has_thought,
                &mut consolidated,
            );
            text.push_str(piece);
        } else {
            flush_text(&mut text, &mut consolidated);
            flush_thought(
                &mut thought,
                &mut signature,
                &mut has_thought,
                &mut consolidated,
            );
            consolidated.push(part);
        }
    }
    flush_thought(
        &mut thought,
        &mut signature,
        &mut has_thought,
        &mut consolidated,
    );
    flush_text(&mut text, &mut consolidated);
    consolidated
}

#[cfg(test)]
mod tests;
