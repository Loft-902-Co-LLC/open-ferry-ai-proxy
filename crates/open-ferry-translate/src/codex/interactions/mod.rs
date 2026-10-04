// Ported from CLIProxyAPI internal/translator/codex/interactions
// (ConvertInteractionsRequestToCodex, ConvertCodexResponseToInteractions,
// ConvertCodexResponseToInteractionsNonStream) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini Interactions clients talking to a Codex upstream.
//!
//! Not ported yet: WP4-D puts the request translator in `request.rs` and the
//! stream and non-streaming response translators in `response.rs`,
//! re-exported from here.
//!
//! Deviations from upstream: none yet.
