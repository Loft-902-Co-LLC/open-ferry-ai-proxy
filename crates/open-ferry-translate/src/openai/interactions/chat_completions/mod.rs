// Ported from CLIProxyAPI internal/translator/openai/interactions/chat-completions
// (ConvertOpenAIRequestToInteractions, ConvertInteractionsResponseToOpenAI,
// ConvertInteractionsResponseToOpenAINonStream, ConvertInteractionsRequestToOpenAI,
// ConvertOpenAIResponseToInteractions, ConvertOpenAIResponseToInteractionsNonStream)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Chat Completions clients talking to a Gemini Interactions upstream, and
//! Interactions clients talking to a Chat Completions upstream.
//!
//! The translators to Interactions (a Chat Completions request, and a Chat
//! Completions response or chunk stream) are in `to_interactions.rs`, and
//! those from Interactions (an Interactions request, and an Interactions
//! response or event stream) in `from_interactions.rs`.
//!
//! Deviations from upstream:
//! - Upstream's Antigravity branches (`isAntigravityModel`) are not ported:
//!   the `external_` tool renames through `AntigravityToolNameToUpstream`
//!   and `AntigravityUpstreamToolNameToClient`, in tools, tool choices,
//!   tool calls and results both ways, and the generation settings dropped
//!   for such a model in favour of `agent_config.max_total_tokens`. A model
//!   whose name holds `antigravity` is translated like any other.
//! - A request, response or stream payload that isn't valid JSON, such as a
//!   stream frame with more than one `data:` line of JSON, is read as having
//!   no fields. gjson reads what it can.
//! - Where upstream copies a value's JSON text, into the output or into a
//!   string (a tool call's arguments, a tool result, a non-string value read
//!   as text), we write the same JSON compactly.
//! - A count or index too large for `i64`, such as `1e30`, saturates. Go's
//!   result depends on the CPU; amd64 gives the minimum `i64`.
//! - When a key is repeated, the last one counts; gjson reads the first.

mod common;
mod from_interactions;
mod to_interactions;

pub use from_interactions::{
    InteractionsToOpenAIStream, convert_interactions_request_to_openai,
    convert_interactions_response_to_openai_non_stream,
};
pub use to_interactions::{
    OpenAIToInteractionsStream, convert_openai_request_to_interactions,
    convert_openai_response_to_interactions_non_stream,
};
