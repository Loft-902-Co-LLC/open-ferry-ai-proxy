// Ported from CLIProxyAPI internal/translator/codex/interactions
// (ConvertInteractionsRequestToCodex, ConvertCodexResponseToInteractions,
// ConvertCodexResponseToInteractionsNonStream) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini Interactions clients talking to a Codex upstream: the request
//! translator in [`request`], and the stream and non-streaming response
//! translators in [`response`].
//!
//! Deviations from upstream: see [`request`] and [`response`].

pub mod request;
pub mod response;

pub use request::convert_interactions_request_to_codex;
pub use response::{CodexToInteractionsStream, convert_codex_response_to_interactions_non_stream};

#[cfg(test)]
mod tests;
