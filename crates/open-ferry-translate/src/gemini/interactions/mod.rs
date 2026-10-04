// Ported from CLIProxyAPI internal/translator/gemini/interactions
// (ConvertInteractionsRequestToGemini, ConvertGeminiRequestToInteractions,
// ConvertGeminiResponseToInteractions, ConvertGeminiResponseToInteractionsNonStream,
// ConvertInteractionsResponseToGemini, ConvertInteractionsResponseToGeminiNonStream,
// ConvertInteractionsRequestToInteractions, ConvertInteractionsResponsePassthrough,
// ConvertInteractionsResponsePassthroughNonStream) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini Interactions clients talking to a Gemini upstream, Gemini clients
//! talking to an Interactions upstream, and Interactions passed through to an
//! Interactions upstream.
//!
//! Not ported yet: WP4-E puts the request translators in `request.rs`, the
//! response translators in `response.rs`, the passthrough in `passthrough.rs`
//! and the snake_case and camelCase key conversion in `case.rs`, re-exported
//! from here.
//!
//! Deviations from upstream: none yet.
