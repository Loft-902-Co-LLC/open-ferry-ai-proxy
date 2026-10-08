// Ported from CLIProxyAPI internal/translator/openai/interactions/responses/interactions_openai_responses_response.go
// (ConvertInteractionsResponseToOpenAIResponses,
// ConvertInteractionsResponseToOpenAIResponsesNonStream,
// ConvertOpenAIResponsesResponseToInteractions,
// ConvertOpenAIResponsesResponseToInteractionsNonStream, FinalizeToolInput,
// and the apply_patch bridge) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini Interactions streams and responses to OpenAI Responses events and
//! objects, and back.
//!
//! [`InteractionsToOpenAIResponsesStream`] and
//! [`convert_interactions_response_to_openai_responses_non_stream`] answer an
//! OpenAI Responses client from an Interactions upstream (see
//! [`to_responses`]). [`OpenAIResponsesToInteractionsStream`] and
//! [`convert_openai_responses_response_to_interactions_non_stream`] answer an
//! Interactions client from a Responses upstream (see [`to_interactions`]).
//!
//! A call to the Responses client's `apply_patch` custom tool has its patch
//! text decoded from the arguments, as the other Responses translators do.
//! Arguments that aren't one valid input string, snapshots of a call that
//! disagree, a call whose identity can't be settled, a failed upstream
//! interaction and a stream that ends early all end the response with
//! `response.failed`; a whole response then gives nothing. After the stream,
//! [`finalize_tool_input`](InteractionsToOpenAIResponsesStream::finalize_tool_input)
//! fails it if an `apply_patch` call never completed, and
//! [`tool_input_error`](InteractionsToOpenAIResponsesStream::tool_input_error)
//! says why it failed.
//!
//! Deviations from upstream:
//! - Upstream's `isAntigravityModel` branches are not ported: a model whose
//!   name contains `antigravity` is translated as any other, with no tool
//!   name mapped for Antigravity.
//! - Text that isn't valid JSON is read up to its first error, and where an
//!   object has a key twice the last one counts (see [`read`]). Text that
//!   isn't UTF-8 is read with each invalid sequence as U+FFFD.
//! - A non-string value read as text, such as a call's `arguments` object,
//!   is written as compact JSON, where upstream uses its JSON text: `{ }`
//!   becomes `{}`. Strings are escaped as serde_json escapes them.
//! - A count or index past the range of `i64` is read as its nearest end.
//! - A step's `delta` in an event that isn't valid JSON is translated from
//!   what was read, not by editing the event's text as upstream does.
//! - A whole response's `output` lists the items at the indexes the steps
//!   used, from 0 up; upstream also tries every index below the largest one
//!   used, which only ever finds those.
//! - The request is read once, when the stream is made, and a `null`
//!   request counts as none.
//! - A tool input error says the arguments were invalid in fewer words than
//!   upstream's: [`InputError`](crate::apply_patch::input::InputError)'s.
//! - Upstream's `FunctionCallIndexes`, which nothing reads, is not kept.
//! - Each stream's `translate` returns one event's frames joined, where
//!   upstream returns them apart; the registry splits them again.

mod items;
mod read;
mod to_interactions;
mod to_responses;

#[cfg(test)]
mod tests;

pub use to_interactions::{
    OpenAIResponsesToInteractionsStream,
    convert_openai_responses_response_to_interactions_non_stream,
};
pub use to_responses::{
    InteractionsToOpenAIResponsesStream,
    convert_interactions_response_to_openai_responses_non_stream,
};
