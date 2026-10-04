// Ported from CLIProxyAPI internal/translator/interactions/claude
// (ConvertClaudeRequestToInteractions, ConvertClaudeRequestToInteractionsWithCompat,
// ConvertInteractionsResponseToClaude, ConvertInteractionsResponseToClaudeNonStream)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Claude Messages clients talking to a Gemini Interactions upstream.
//!
//! Not ported yet: WP4-A puts the request translator (and its compatibility
//! mode) in `request.rs` and the stream and non-streaming response
//! translators in `response.rs`, re-exported from here.
//!
//! Deviations from upstream: none yet.
