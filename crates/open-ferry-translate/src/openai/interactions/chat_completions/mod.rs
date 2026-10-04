// Ported from CLIProxyAPI internal/translator/openai/interactions/chat-completions
// (ConvertOpenAIRequestToInteractions, ConvertInteractionsResponseToOpenAI,
// ConvertInteractionsResponseToOpenAINonStream, ConvertInteractionsRequestToOpenAI,
// ConvertOpenAIResponseToInteractions, ConvertOpenAIResponseToInteractionsNonStream)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Chat Completions clients talking to a Gemini Interactions upstream, and
//! Interactions clients talking to a Chat Completions upstream.
//!
//! Not ported yet: WP4-B puts the translators to Interactions in
//! `to_interactions.rs` and those from Interactions in `from_interactions.rs`,
//! re-exported from here.
//!
//! Deviations from upstream: none yet.
