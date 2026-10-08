// Ported from CLIProxyAPI internal/translator/claude/interactions
// (ConvertInteractionsRequestToClaude, ConvertClaudeResponseToInteractions,
// ConvertClaudeResponseToInteractionsNonStream) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini Interactions clients talking to a Claude upstream.
//!
//! The request translator is in `request.rs`, and the stream and
//! non-streaming response translators in `response.rs`.
//!
//! Deviations from upstream: see each module.

mod request;
mod response;

pub use request::convert_interactions_request_to_claude;
pub use response::{
    ClaudeToInteractionsStream, convert_claude_response_to_interactions_non_stream,
};
