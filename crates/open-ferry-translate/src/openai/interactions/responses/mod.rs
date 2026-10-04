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
//! line right below its `mod` line.
//!
//! Deviations from upstream: each file lists its own.

mod request;
pub use request::{
    convert_interactions_request_to_openai_responses,
    convert_openai_responses_request_to_interactions,
};

mod response;
pub use response::{
    InteractionsToOpenAIResponsesStream, OpenAIResponsesToInteractionsStream,
    convert_interactions_response_to_openai_responses_non_stream,
    convert_openai_responses_response_to_interactions_non_stream,
};
