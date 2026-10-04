// Ported from CLIProxyAPI internal/translator/openai/interactions/responses
// (ConvertOpenAIResponsesRequestToInteractions, ConvertInteractionsRequestToOpenAIResponses,
// ConvertInteractionsResponseToOpenAIResponses,
// ConvertInteractionsResponseToOpenAIResponsesNonStream,
// ConvertOpenAIResponsesResponseToInteractions,
// ConvertOpenAIResponsesResponseToInteractionsNonStream) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! OpenAI Responses clients talking to a Gemini Interactions upstream, and
//! Interactions clients talking to an OpenAI Responses upstream.
//!
//! The request translators, both ways, are in `request.rs`, and the response
//! translators, both ways, in `response.rs`, each re-exported by a `pub use`
//! line right below its `mod` line. Not ported yet: WP4-C fills both (WP4-C1
//! the requests and WP4-C2 the responses, if it is split).
//!
//! Deviations from upstream: none yet.

mod request;

mod response;
