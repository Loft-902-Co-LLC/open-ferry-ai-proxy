//! Our side of the executor helpers' harness's `ttft/token-event` entry:
//! whether an upstream's stream event carries the first token, as
//! `open_ferry_core::observe::usage` decides it for the time to first
//! token (see `go/helps/parity_ttft.go`).

use open_ferry_core::observe::usage::{
    is_chat_token_event, is_claude_token_event, is_gemini_token_event, is_responses_token_event,
};
use serde_json::Value;

use crate::cases::Case;

/// `ttft/token-event`: whether the case's event carries a token, for the
/// protocol its `format` option names.
pub fn token_event(case: &Case) -> Result<Value, String> {
    let payload = case.request.as_bytes();
    let token = match case.options["format"].as_str().unwrap_or_default() {
        "responses" => is_responses_token_event(payload),
        "chat" => is_chat_token_event(payload),
        "claude" => is_claude_token_event(payload),
        "gemini" => is_gemini_token_event(payload),
        other => return Err(format!("case {}: unknown format {other:?}", case.name)),
    };
    Ok(Value::Bool(token))
}
