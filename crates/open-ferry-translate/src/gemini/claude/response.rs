// Ported from CLIProxyAPI internal/translator/gemini/claude/gemini_claude_response.go
// (ConvertGeminiResponseToClaude, ConvertGeminiResponseToClaudeNonStream and
// ClaudeTokenCount) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini responses → Claude Messages responses.
//!
//! [`GeminiToClaudeStream`] turns each chunk of a Gemini stream into Claude
//! SSE events, and [`convert_gemini_response_to_claude_non_stream`] turns a
//! whole Gemini response into one Claude message. Thought parts become
//! thinking blocks, with their signature; function calls become `tool_use`
//! blocks, under the name the client declared.
//!
//! Deviations from upstream:
//! - A chunk or body that is not valid JSON is treated as having no fields.
//!   gjson reads what it can from malformed JSON.
//! - A function call's `args` streams as compact JSON in `partial_json`;
//!   upstream copies Gemini's JSON text.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use crate::common::claude::sanitize_tool_id;
use crate::common::gemini::{
    SanitizedToolNames, restore_sanitized_tool_name, sanitized_tool_name_map,
};
use crate::common::tool_names::{ToolNameMap, map_tool_name, tool_name_map_from_claude_request};
use crate::gemini_schema::get;
use crate::json::{bool_of, int_of, path, str_of};

/// Numbers the `tool_use` blocks of every stream in the process, as
/// upstream's package-wide counter does.
static TOOL_USE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The message ID and model a stream starts with when Gemini gives none.
const DEFAULT_MESSAGE_ID: &str = "msg_1nZdL29xx5MUA1yADyHTEsnR8uuvGzszyY";
const DEFAULT_MODEL: &str = "claude-3-5-sonnet-20241022";

/// Converts a Claude `count_tokens` result into its response body.
pub fn claude_token_count(count: i64) -> Value {
    json!({ "input_tokens": count })
}

/// The kind of content block a stream has open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Block {
    None,
    Text,
    Thinking,
    ToolUse,
}

/// Translates a Gemini response stream into Claude Messages SSE events, one
/// chunk at a time. Keep one per response: it tracks which content block is
/// open.
pub struct GeminiToClaudeStream {
    /// Canonical tool names → the names the client declared.
    tool_names: Option<ToolNameMap>,
    /// Sanitized function names → the names the client declared.
    sanitized_names: Option<SanitizedToolNames>,
    has_first_response: bool,
    block: Block,
    block_index: usize,
    /// Whether any content block has been sent.
    has_content: bool,
    saw_tool_call: bool,
    has_final_events: bool,
}

impl GeminiToClaudeStream {
    /// `original_request` is the client's Claude request, used to restore the
    /// tool names it declared.
    pub fn new(original_request: &Value) -> Self {
        Self {
            tool_names: tool_name_map_from_claude_request(original_request),
            sanitized_names: sanitized_tool_name_map(original_request),
            has_first_response: false,
            block: Block::None,
            block_index: 0,
            has_content: false,
            saw_tool_call: false,
            has_final_events: false,
        }
    }

    /// Translates one chunk of the Gemini stream: a response's JSON, or
    /// `[DONE]` at the end. Returns the Claude SSE events it gives
    /// (`event: …\ndata: …\n\n\n` each), possibly none.
    pub fn translate(&mut self, chunk: &[u8]) -> String {
        let mut out = String::new();
        if chunk == b"[DONE]" {
            // A message without content gets no stop event.
            if self.has_content {
                push_event(&mut out, "message_stop", &json!({"type": "message_stop"}));
            }
            return out;
        }
        let response: Value = serde_json::from_slice(chunk).unwrap_or(Value::Null);

        if !self.has_first_response {
            let mut message = json!({
                "id": DEFAULT_MESSAGE_ID,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": DEFAULT_MODEL,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0},
            });
            if let Some(model) = response.get("modelVersion") {
                message["model"] = Value::from(str_of(Some(model)));
            }
            if let Some(id) = response.get("responseId") {
                message["id"] = Value::from(str_of(Some(id)));
            }
            push_event(
                &mut out,
                "message_start",
                &json!({"type": "message_start", "message": message}),
            );
            self.has_first_response = true;
        }

        if let Some(Value::Array(parts)) = get(&response, "candidates.0.content.parts") {
            for part in parts {
                self.translate_part(part, &mut out);
            }
        }

        if let Some(usage) = response.get("usageMetadata")
            && contains(chunk, br#""finishReason""#)
            && !self.has_final_events
            && self.has_content
        {
            if self.block != Block::None {
                self.push_block_stop(&mut out);
                self.block = Block::None;
            }
            let stop_reason = if self.saw_tool_call {
                "tool_use"
            } else if str_of(get(&response, "candidates.0.finishReason")) == "MAX_TOKENS" {
                "max_tokens"
            } else {
                "end_turn"
            };
            let count = |key: &str| usage.get(key).map_or(0, int_of);
            let cached = count("cachedContentTokenCount");
            let mut delta = json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": {
                    "input_tokens": count("promptTokenCount").wrapping_sub(cached).max(0),
                    "output_tokens": count("candidatesTokenCount")
                        .wrapping_add(count("thoughtsTokenCount")),
                },
            });
            if cached > 0 {
                delta["usage"]["cache_read_input_tokens"] = Value::from(cached);
            }
            push_event(&mut out, "message_delta", &delta);
            self.has_final_events = true;
        }
        out
    }

    fn translate_part(&mut self, part: &Value, out: &mut String) {
        let text = part.get("text");
        let function_call = part.get("functionCall");
        let signature = part
            .get("thoughtSignature")
            .or_else(|| part.get("thought_signature"));
        let signature = str_of(signature);
        let has_signature = !signature.is_empty();

        if has_signature && text.is_none() && function_call.is_none() {
            self.push_signature_delta(&signature, out);
            return;
        }

        if let Some(text) = text {
            let text = str_of(Some(text));
            if part.get("thought").is_some_and(bool_of) || has_signature {
                if has_signature && text.is_empty() {
                    self.push_signature_delta(&signature, out);
                    return;
                }
                if self.block != Block::Thinking {
                    self.close_block(out);
                    self.push_block_start(json!({"type": "thinking", "thinking": ""}), out);
                    self.block = Block::Thinking;
                }
                self.push_delta(json!({"type": "thinking_delta", "thinking": text}), out);
                self.has_content = true;
                self.push_signature_delta(&signature, out);
            } else {
                if self.block != Block::Text {
                    self.close_block(out);
                    self.push_block_start(json!({"type": "text", "text": ""}), out);
                    self.block = Block::Text;
                }
                self.push_delta(json!({"type": "text_delta", "text": text}), out);
                self.has_content = true;
            }
        } else if let Some(call) = function_call {
            self.saw_tool_call = true;
            let upstream_name = restore_sanitized_tool_name(
                self.sanitized_names.as_ref(),
                &str_of(call.get("name")),
            );
            let client_name = map_tool_name(self.tool_names.as_ref(), &upstream_name);
            let args = call.get("args");

            // A call without a name continues the open one: Gemini can split
            // a call's arguments across chunks.
            if self.block == Block::ToolUse && upstream_name.is_empty() {
                if let Some(args) = args {
                    self.push_input_delta(args, out);
                }
                return;
            }
            self.close_block(out);
            self.block = Block::None;

            let n = TOOL_USE_ID_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
            self.push_block_start(
                json!({
                    "type": "tool_use",
                    "id": sanitize_tool_id(&format!("{upstream_name}-{n}")),
                    "name": client_name,
                    "input": {},
                }),
                out,
            );
            if let Some(args) = args {
                self.push_input_delta(args, out);
            }
            self.block = Block::ToolUse;
            self.has_content = true;
        }
    }

    /// Stops the open content block, if there is one, and moves to the next
    /// index.
    fn close_block(&mut self, out: &mut String) {
        if self.block != Block::None {
            self.push_block_stop(out);
            self.block_index += 1;
        }
    }

    fn push_block_start(&self, content_block: Value, out: &mut String) {
        push_event(
            out,
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": self.block_index,
                "content_block": content_block,
            }),
        );
    }

    fn push_block_stop(&self, out: &mut String) {
        push_event(
            out,
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": self.block_index}),
        );
    }

    fn push_delta(&self, delta: Value, out: &mut String) {
        push_event(
            out,
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": self.block_index, "delta": delta}),
        );
    }

    fn push_input_delta(&self, args: &Value, out: &mut String) {
        self.push_delta(
            json!({"type": "input_json_delta", "partial_json": args.to_string()}),
            out,
        );
    }

    /// Sends a thought signature, if there is one and a thinking block is
    /// open.
    fn push_signature_delta(&mut self, signature: &str, out: &mut String) {
        if signature.is_empty() || self.block != Block::Thinking {
            return;
        }
        self.push_delta(
            json!({"type": "signature_delta", "signature": signature}),
            out,
        );
        self.has_content = true;
    }
}

/// Converts a whole Gemini response into a Claude message. Text and thought
/// parts in a row are joined into one block. `original_request` is the
/// client's Claude request, used to restore the tool names it declared.
pub fn convert_gemini_response_to_claude_non_stream(
    original_request: &Value,
    response: &Value,
) -> Value {
    let tool_names = tool_name_map_from_claude_request(original_request);
    let sanitized_names = sanitized_tool_name_map(original_request);

    let count = |path_: &str| path(response, path_).map_or(0, int_of);
    let cached = count("usageMetadata.cachedContentTokenCount");
    let input_tokens = count("usageMetadata.promptTokenCount")
        .wrapping_sub(cached)
        .max(0);
    let output_tokens = count("usageMetadata.candidatesTokenCount")
        .wrapping_add(count("usageMetadata.thoughtsTokenCount"));
    let mut out = json!({
        "id": str_of(response.get("responseId")),
        "type": "message",
        "role": "assistant",
        "model": str_of(response.get("modelVersion")),
        "content": [],
        "stop_reason": null,
        "stop_sequence": null,
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
    });
    if cached > 0 {
        out["usage"]["cache_read_input_tokens"] = Value::from(cached);
    }

    let mut blocks = Blocks::default();
    let mut tool_count = 0;
    if let Some(Value::Array(parts)) = get(response, "candidates.0.content.parts") {
        for part in parts {
            let signature = str_of(
                part.get("thoughtSignature")
                    .or_else(|| part.get("thought_signature")),
            );
            let has_signature = !signature.is_empty();
            if has_signature {
                blocks.thinking_signature = signature.into_owned();
            }
            let text = str_of(part.get("text"));
            let function_call = part.get("functionCall");
            if has_signature && text.is_empty() && function_call.is_none() {
                continue;
            }
            if !text.is_empty() {
                if part.get("thought").is_some_and(bool_of) || has_signature {
                    blocks.flush_text();
                    blocks.thinking.push_str(&text);
                } else {
                    blocks.flush_thinking();
                    blocks.text.push_str(&text);
                }
                continue;
            }
            if let Some(call) = function_call {
                blocks.flush_thinking();
                blocks.flush_text();
                blocks.has_tool_call = true;
                let upstream_name = restore_sanitized_tool_name(
                    sanitized_names.as_ref(),
                    &str_of(call.get("name")),
                );
                tool_count += 1;
                let input = match call.get("args") {
                    Some(args @ Value::Object(_)) => args.clone(),
                    _ => json!({}),
                };
                blocks.blocks.push(json!({
                    "type": "tool_use",
                    "id": sanitize_tool_id(&format!("{upstream_name}-{tool_count}")),
                    "name": map_tool_name(tool_names.as_ref(), &upstream_name),
                    "input": input,
                }));
            }
        }
    }
    blocks.flush_thinking();
    blocks.flush_text();
    if !blocks.blocks.is_empty() {
        out["content"] = Value::Array(blocks.blocks);
    }

    out["stop_reason"] = Value::from(if blocks.has_tool_call {
        "tool_use"
    } else if str_of(get(response, "candidates.0.finishReason")) == "MAX_TOKENS" {
        "max_tokens"
    } else {
        "end_turn"
    });

    if input_tokens == 0 && output_tokens == 0 && response.get("usageMetadata").is_none() {
        out.as_object_mut()
            .expect("built as an object")
            .shift_remove("usage");
    }
    out
}

/// The content blocks of a non-streaming message, and the text and thinking
/// still being gathered.
#[derive(Default)]
struct Blocks {
    blocks: Vec<Value>,
    text: String,
    thinking: String,
    thinking_signature: String,
    has_tool_call: bool,
}

impl Blocks {
    fn flush_text(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text);
        self.blocks.push(json!({"type": "text", "text": text}));
    }

    fn flush_thinking(&mut self) {
        if self.thinking.is_empty() && self.thinking_signature.is_empty() {
            return;
        }
        let mut block = json!({"type": "thinking", "thinking": std::mem::take(&mut self.thinking)});
        let signature = std::mem::take(&mut self.thinking_signature);
        if !signature.is_empty() {
            block["signature"] = Value::String(signature);
        }
        self.blocks.push(block);
    }
}

fn push_event(out: &mut String, event: &str, data: &Value) {
    let _ = write!(out, "event: {event}\ndata: {data}\n\n\n");
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests;
