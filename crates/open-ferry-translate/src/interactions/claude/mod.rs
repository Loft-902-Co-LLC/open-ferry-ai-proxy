// Ported from CLIProxyAPI internal/translator/interactions/claude
// (ConvertClaudeRequestToInteractions, ConvertClaudeRequestToInteractionsWithCompat,
// ConvertInteractionsResponseToClaude, ConvertInteractionsResponseToClaudeNonStream)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Claude Messages clients talking to a Gemini Interactions upstream.
//!
//! The request translator (and its compatibility mode) is in `request.rs`,
//! and the stream and non-streaming response translators in `response.rs`.
//!
//! Deviations from upstream: see each module.

mod request;
mod response;

pub use request::{
    convert_claude_request_to_interactions, convert_claude_request_to_interactions_with_compat,
};
pub use response::{
    InteractionsToClaudeStream, convert_interactions_response_to_claude_non_stream,
};
