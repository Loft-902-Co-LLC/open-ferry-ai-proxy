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
//! The request translators are in `request.rs`, the response translators in
//! `response.rs`, the passthrough in `passthrough.rs`, and the snake_case and
//! camelCase keys of a generation config in `case.rs`.
//!
//! Deviations from upstream:
//! - A chunk or body that is not valid JSON is treated as having no fields.
//!   Upstream's gjson reads what it can of it.
//! - JSON that upstream copies as text is written compact, with its keys in
//!   their order: the `arguments` of a stream's `arguments_delta`, a function
//!   result holding a `$ref` (stored as a string), and an object where
//!   upstream wants a string (a `thinking_level`, a text, a signature or a
//!   description).
//! - Arguments or a result given as a string that gjson finds valid but
//!   serde_json can't parse (a lone surrogate escape, or nesting deeper than
//!   128) stay `{}`, or the string, where upstream copies the text.
//! - Generation config keys holding `|`, `#`, `@`, `*` or `?`, and digit keys
//!   that would pad an array with more than 65,535 nulls or index into an
//!   array they don't reach, are left out, where upstream's sjson reads a
//!   query, runs out of memory or panics.
//! - A generation config leaf whose path sjson reads as more than 128 keys (a
//!   key of many dots, say) is left out, where upstream nests a value for
//!   each key, however many.

mod case;
mod passthrough;
mod request;
mod response;

pub use passthrough::{
    convert_interactions_request_to_interactions, convert_interactions_response_passthrough,
    convert_interactions_response_passthrough_non_stream,
};
pub use request::{convert_gemini_request_to_interactions, convert_interactions_request_to_gemini};
pub use response::{
    GeminiToInteractionsStream, InteractionsToGeminiStream,
    convert_gemini_response_to_interactions_non_stream,
    convert_interactions_response_to_gemini_non_stream,
};
