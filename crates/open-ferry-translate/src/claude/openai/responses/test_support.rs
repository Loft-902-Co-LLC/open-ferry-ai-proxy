// Ported from CLIProxyAPI internal/translator/claude/openai/responses/claude_openai-responses_testsupport_test.go
// and helpers in its other test files (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Fixtures shared by this translator's tests.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE};
use serde_json::{Value, json};

use crate::json::str_of;
use crate::signature::{Provider, compatible_signature_for_provider};

/// Appends a protobuf varint.
fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn varint_field(out: &mut Vec<u8>, field: u64, value: u64) {
    varint(out, field << 3);
    varint(out, value);
}

fn bytes_field(out: &mut Vec<u8>, field: u64, value: &[u8]) {
    varint(out, field << 3 | 2);
    varint(out, value.len() as u64);
    out.extend_from_slice(value);
}

/// `testClaudeResponsesThinkingSignatureForModel`: a Claude thinking
/// signature naming `model`, as the client would send it back, and as Claude
/// should get it.
pub(super) fn claude_thinking_signature_for_model(model: &str) -> (String, String) {
    let mut channel = Vec::new();
    varint_field(&mut channel, 1, 12);
    varint_field(&mut channel, 2, 2);
    bytes_field(&mut channel, 6, model.as_bytes());
    let mut container = Vec::new();
    bytes_field(&mut container, 1, &channel);
    let mut payload = Vec::new();
    bytes_field(&mut payload, 2, &container);
    varint_field(&mut payload, 3, 1);

    let raw = STANDARD.encode(payload);
    let normalized = compatible_signature_for_provider(Provider::Claude, &raw)
        .expect("test Claude signature should be compatible");
    (raw, normalized)
}

/// `testClaudeResponsesThinkingSignature`: a signature from
/// `claude-sonnet-4-6`.
pub(super) fn claude_thinking_signature() -> (String, String) {
    claude_thinking_signature_for_model("claude-sonnet-4-6")
}

/// `mustTestSignature`: the raw form of [`claude_thinking_signature`].
pub(super) fn test_signature() -> String {
    claude_thinking_signature().0
}

/// `testGPTResponsesReasoningSignature`: a GPT reasoning signature.
pub(super) fn gpt_reasoning_signature() -> String {
    let mut payload = [0u8; 1 + 8 + 16 + 16 + 32];
    payload[0] = 0x80;
    payload[8] = 1;
    for (i, byte) in payload.iter_mut().enumerate().skip(9) {
        *byte = i as u8;
    }
    URL_SAFE.encode(payload)
}

/// `responsesRequestFromItems`: a request for `claude-test` with these input
/// items.
pub(super) fn request_from_items(items: impl IntoIterator<Item = Value>) -> Value {
    json!({"model": "claude-test", "input": items.into_iter().collect::<Vec<_>>()})
}

/// `claudeAssistantBlockTypes`: the block types of the last assistant
/// message in a translated Claude request.
pub(super) fn assistant_block_types(claude_request: &Value) -> Vec<String> {
    let Some(Value::Array(messages)) = claude_request.get("messages") else {
        return Vec::new();
    };
    let Some(last) = messages
        .iter()
        .rev()
        .find(|message| str_of(message.get("role")) == "assistant")
    else {
        return Vec::new();
    };
    match last.get("content") {
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| str_of(block.get("type")).into_owned())
            .collect(),
        _ => Vec::new(),
    }
}

/// Splits translator output into its SSE events: each event's name and data.
pub(super) fn sse_events(output: &str) -> Vec<(String, Value)> {
    output
        .split("\n\n")
        .filter(|frame| !frame.trim().is_empty())
        .map(parse_sse_event)
        .collect()
}

/// `parseClaudeResponsesSSEEvent`: one SSE frame's event name and data.
pub(super) fn parse_sse_event(frame: &str) -> (String, Value) {
    let mut event = String::new();
    let mut data = None;
    for line in frame.split('\n') {
        if let Some(name) = line.strip_prefix("event: ") {
            event = name.to_owned();
        } else if let Some(payload) = line.strip_prefix("data: ") {
            data = Some(payload);
        }
    }
    let data = data.unwrap_or_else(|| panic!("SSE frame has no data line: {frame}"));
    let data = serde_json::from_str(data).unwrap_or_else(|err| panic!("bad data {data}: {err}"));
    (event, data)
}
