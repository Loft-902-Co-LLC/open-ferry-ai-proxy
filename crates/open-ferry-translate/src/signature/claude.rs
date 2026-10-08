// Ported from CLIProxyAPI internal/signature/claude.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Removing Claude thinking blocks that Claude would reject.

use serde_json::Value;

use super::{ClaudeValidationOptions, is_valid_claude_thinking_signature};
use crate::json::str_of;

/// `StripInvalidClaudeThinkingBlocks`: removes thinking blocks from `messages`
/// whose signature, after any cache prefix, isn't a valid Claude thinking
/// signature. Returns whether any were removed.
///
/// With `allow_empty_signature_with_empty_text`, a block with neither a
/// signature nor text is kept.
pub fn strip_invalid_claude_thinking_blocks(
    payload: &mut Value,
    opt: ClaudeValidationOptions,
) -> bool {
    let Some(Value::Array(messages)) = payload.get_mut("messages") else {
        return false;
    };
    let mut modified = false;
    for message in messages {
        let Some(Value::Array(content)) = message.get_mut("content") else {
            continue;
        };
        let before = content.len();
        content.retain(|part| {
            !(str_of(part.get("type")) == "thinking"
                && should_strip_claude_thinking_block(part, opt))
        });
        modified |= content.len() != before;
    }
    modified
}

/// `StripInvalidClaudeThinkingBlocksAndEmptyMessages`: like
/// [`strip_invalid_claude_thinking_blocks`], and when that removes anything,
/// also removes every message whose content is an empty array.
pub fn strip_invalid_claude_thinking_blocks_and_empty_messages(
    payload: &mut Value,
    opt: ClaudeValidationOptions,
) -> bool {
    if !strip_invalid_claude_thinking_blocks(payload, opt) {
        return false;
    }
    if let Some(Value::Array(messages)) = payload.get_mut("messages") {
        messages.retain(|message| {
            !matches!(message.get("content"), Some(Value::Array(content)) if content.is_empty())
        });
    }
    true
}

fn should_strip_claude_thinking_block(part: &Value, opt: ClaudeValidationOptions) -> bool {
    if opt.allow_empty_signature_with_empty_text && is_empty_claude_thinking_placeholder(part) {
        return false;
    }
    !is_valid_claude_thinking_signature(&str_of(part.get("signature")), opt)
}

/// Whether a thinking block has neither a signature nor any text.
pub(super) fn is_empty_claude_thinking_placeholder(part: &Value) -> bool {
    str_of(part.get("signature")).trim().is_empty()
        && claude_thinking_block_text(part).trim().is_empty()
}

/// The text of a thinking block, which clients put in `text`, `thinking`, or an
/// object under `thinking`.
fn claude_thinking_block_text(part: &Value) -> &str {
    if let Some(Value::String(text)) = part.get("text") {
        return text;
    }
    match part.get("thinking") {
        Some(Value::String(thinking)) => thinking,
        Some(Value::Object(thinking)) => match (thinking.get("text"), thinking.get("thinking")) {
            (Some(Value::String(text)), _) | (_, Some(Value::String(text))) => text,
            _ => "",
        },
        _ => "",
    }
}
