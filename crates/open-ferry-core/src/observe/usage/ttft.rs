// Ported from CLIProxyAPI internal/runtime/executor/helps/
// responses_ttft_helpers.go (IsResponsesTokenEvent), chat_ttft_helpers.go
// (IsChatTokenEvent), claude_ttft_helpers.go (IsClaudeTokenEvent),
// gemini_ttft_helpers.go (IsGeminiTokenEvent) and usage_helpers.go
// (StartResponseTTFT, IsTTFTSet, IsFirstPacketSet, RecordFirstPacket,
// ObserveTokenEvent, MarkFirstResponseByte, setTTFT, ttftDuration)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Time to first token: how long an upstream attempt took to give its first
//! token, or failing that its first packet, or its first byte.
//!
//! A stream read event by event marks the time at the first event that
//! carries output: text, reasoning, a tool call's arguments, audio or an
//! image, or the event that ends the answer. Each protocol has its
//! classifier: [`is_responses_token_event`] for the Responses API and
//! Codex, [`is_chat_token_event`] for Chat Completions,
//! [`is_claude_token_event`] for Claude's Messages and
//! [`is_gemini_token_event`] for Gemini. Until a token comes, the first
//! event's time stands in. An answer read whole marks the time of its first
//! byte.
//!
//! Deviations from upstream: JSON that doesn't parse whole carries no
//! token (see [`super::json`]).

use std::time::{Duration, Instant};

use super::json::{self, Doc};

/// `payload` without white space and a `data:` prefix; `None` when nothing
/// is left after the prefix.
fn sse_data(payload: &[u8]) -> Option<&[u8]> {
    let payload = json::trim_space(payload);
    if payload.is_empty() {
        return None;
    }
    let Some(rest) = payload.strip_prefix(b"data:") else {
        return Some(payload);
    };
    let rest = json::trim_space(rest.strip_prefix(b" ").unwrap_or(rest));
    (!rest.is_empty()).then_some(rest)
}

/// Whether the value at `path` has text.
fn has(doc: &Doc, path: &str) -> bool {
    !doc.get(path).string().is_empty()
}

/// Whether a Responses API event (an SSE line or a WebSocket message)
/// carries output: a non-empty delta of text, reasoning, a tool call,
/// audio or an image, a finished item with content, or the event that ends
/// the answer (upstream's `IsResponsesTokenEvent`).
pub fn is_responses_token_event(payload: &[u8]) -> bool {
    let Some(payload) = sse_data(payload) else {
        return false;
    };
    let doc = Doc::scan(payload);
    match doc.get("type").string().as_ref() {
        "response.reasoning_summary_text.delta"
        | "response.reasoning.delta"
        | "response.reasoning_text.delta"
        | "response.output_text.delta"
        | "response.text.delta"
        | "response.function_call_arguments.delta"
        | "response.custom_tool_call_input.delta"
        | "response.code_interpreter_call_code.delta"
        | "response.mcp_call_arguments.delta"
        | "response.shell_call_command.delta"
        | "response.refusal.delta"
        | "response.audio.transcript.delta" => has(&doc, "delta"),
        "response.audio.delta" => has(&doc, "delta") || has(&doc, "data"),
        "response.image_generation_call.partial_image" => has(&doc, "partial_image_b64"),
        "response.shell_call_command.added" | "response.shell_call_command.done" => {
            has(&doc, "command")
        }
        "response.reasoning_summary_text.done"
        | "response.reasoning_text.done"
        | "response.output_text.done" => has(&doc, "text"),
        "response.refusal.done" => has(&doc, "refusal"),
        "response.function_call_arguments.done" | "response.mcp_call_arguments.done" => {
            has(&doc, "arguments")
        }
        "response.custom_tool_call_input.done" => has(&doc, "input"),
        "response.code_interpreter_call_code.done" => has(&doc, "code"),
        "response.reasoning_summary_part.done" => has(&doc, "part.text"),
        "response.content_part.done" => has(&doc, "part.text") || has(&doc, "part.refusal"),
        "response.output_item.done" => match doc.get("item.type").string().as_ref() {
            "function_call" => has(&doc, "item.arguments"),
            "custom_tool_call" => has(&doc, "item.input"),
            "message" => doc.get("item.content").array().into_iter().any(|content| {
                !content.get("text").string().is_empty()
                    || !content.get("refusal").string().is_empty()
            }),
            _ => false,
        },
        "response.completed"
        | "response.done"
        | "response.incomplete"
        | "response.failed"
        | "error" => true,
        _ => false,
    }
}

/// Whether a Chat Completions chunk carries output: content, reasoning, a
/// refusal or a tool call in a choice's delta or message, a finish reason,
/// an error, or `[DONE]` (upstream's `IsChatTokenEvent`).
pub fn is_chat_token_event(payload: &[u8]) -> bool {
    let trimmed = json::trim_space(payload);
    if trimmed == b"data: [DONE]" || trimmed == b"[DONE]" {
        return true;
    }
    let Some(payload) = sse_data(trimmed) else {
        return false;
    };
    if payload == b"[DONE]" {
        return true;
    }
    let doc = Doc::scan(payload);
    if doc.get("error.message").exists() || doc.get("error").exists() {
        return true;
    }
    let text = |node: json::Node<'_>, path: &str| !node.get(path).string().is_empty();
    doc.get("choices").array().into_iter().any(|choice| {
        let delta = choice.get("delta");
        if delta.exists() {
            if ["content", "reasoning_content", "reasoning", "refusal"]
                .iter()
                .any(|path| text(delta, path))
            {
                return true;
            }
            if delta.get("tool_calls").array().into_iter().any(|call| {
                text(call, "function.arguments")
                    || text(call, "function.name")
                    || text(call, "custom.input")
            }) {
                return true;
            }
        }
        let message = choice.get("message");
        if message.exists() {
            if ["content", "reasoning_content", "refusal"]
                .iter()
                .any(|path| text(message, path))
            {
                return true;
            }
            if message
                .get("tool_calls")
                .array()
                .into_iter()
                .any(|call| text(call, "function.arguments") || text(call, "function.name"))
            {
                return true;
            }
        }
        text(choice, "finish_reason")
    })
}

/// Whether a Claude Messages event carries output: a non-empty block delta
/// or block start, a stop reason, the stop or an error, or a whole message
/// with content (upstream's `IsClaudeTokenEvent`).
pub fn is_claude_token_event(payload: &[u8]) -> bool {
    let mut payload = json::trim_space(payload);
    if payload.starts_with(b"event:")
        && let Some(newline) = payload.iter().position(|&b| b == b'\n')
    {
        payload = json::trim_space(payload.get(newline + 1..).unwrap_or_default());
    }
    let Some(payload) = sse_data(payload) else {
        return false;
    };
    let doc = Doc::scan(payload);
    match doc.get("type").string().as_ref() {
        "content_block_delta" => ["text", "thinking", "partial_json", "signature"]
            .iter()
            .any(|field| has(&doc, &format!("delta.{field}"))),
        "content_block_start" => {
            has(&doc, "content_block.text") || has(&doc, "content_block.thinking")
        }
        "message_delta" => has(&doc, "delta.stop_reason"),
        "message_stop" | "error" => true,
        "message_start" | "ping" | "content_block_stop" => false,
        _ => {
            let content = doc.get("content");
            content.is_array()
                && content.array().into_iter().any(|block| {
                    !block.get("text").string().is_empty()
                        || !block.get("thinking").string().is_empty()
                        || (block.get("type").string() == "tool_use"
                            && !block.get("name").string().is_empty())
                })
        }
    }
}

/// Whether a Gemini chunk carries output: a candidate part with text, a
/// thought, a function call or inline data, a finish reason, or an error
/// (upstream's `IsGeminiTokenEvent`).
pub fn is_gemini_token_event(payload: &[u8]) -> bool {
    let Some(payload) = sse_data(payload) else {
        return false;
    };
    let doc = Doc::scan(payload);
    if doc.get("error.message").exists()
        || doc.get("error").exists()
        || doc.get("response.error").exists()
    {
        return true;
    }
    let mut candidates = doc.get("candidates");
    if !candidates.exists() {
        candidates = doc.get("response.candidates");
    }
    candidates.array().into_iter().any(|candidate| {
        let part_has_output = candidate
            .get("content.parts")
            .array()
            .into_iter()
            .any(|part| {
                let thought = part.get("thought");
                !part.get("text").string().is_empty()
                    || !part.get("thoughtText").string().is_empty()
                    || (thought.is_string() && !thought.string().is_empty())
                    || !part.get("functionCall.name").string().is_empty()
                    || !part.get("inlineData.data").string().is_empty()
            });
        part_has_output || !candidate.get("finishReason").string().is_empty()
    })
}

/// An attempt's time to first token, as the usage reporter measures it.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Ttft {
    start: Option<Instant>,
    ttft: Option<Duration>,
    first_packet: Option<Duration>,
}

impl Ttft {
    /// Starts the clock at `now`, unless it runs or has stopped (upstream's
    /// `StartResponseTTFT`).
    pub(crate) fn start(&mut self, now: Instant) {
        if self.ttft.is_none() && self.start.is_none() {
            self.start = Some(now);
        }
    }

    /// Whether the time to first token is known (upstream's `IsTTFTSet`).
    pub(crate) fn is_set(&self) -> bool {
        self.ttft.is_some()
    }

    /// Whether the first packet's time is known (upstream's
    /// `IsFirstPacketSet`).
    #[cfg(test)]
    pub(crate) fn is_first_packet_set(&self) -> bool {
        self.first_packet.is_some()
    }

    /// An event came at `now`: the first one's time is kept, and a token's
    /// stops the clock (upstream's `ObserveTokenEvent`).
    pub(crate) fn observe_token_event(&mut self, is_token: bool, now: Instant) {
        if self.ttft.is_some() {
            return;
        }
        let Some(start) = self.start else {
            return;
        };
        if !is_token && self.first_packet.is_some() {
            return;
        }
        let elapsed = now.saturating_duration_since(start);
        if self.first_packet.is_none() {
            self.first_packet = Some(elapsed);
        }
        if is_token {
            self.ttft = Some(elapsed);
            self.start = None;
        }
    }

    /// The first byte of the answer came at `now`: it stops the clock
    /// (upstream's `MarkFirstResponseByte`).
    pub(crate) fn mark_first_response_byte(&mut self, now: Instant) {
        if self.ttft.is_some() {
            return;
        }
        if let Some(start) = self.start.take() {
            self.ttft = Some(now.saturating_duration_since(start));
        }
    }

    /// The time to first token, else to the first packet, else zero
    /// (upstream's `ttftDuration`).
    pub(crate) fn duration(&self) -> Duration {
        self.ttft.or(self.first_packet).unwrap_or_default()
    }
}
