//! OpenAI Responses clients talking to an OpenAI Chat Completions upstream.
//! The request becomes a Chat Completions request, and the Chat Completions
//! stream or response becomes Responses events or one Responses object.
//!
//! Upstream's handler for `/v1/chat/completions` also calls the request
//! translator directly, for a client that sends a Responses body there: one
//! with `input` or `instructions` and no `messages`.

mod request;
mod response;
mod shell_tool;
mod tool_index;
mod tools;

pub use request::convert_openai_responses_request_to_openai_chat_completions;
pub(crate) use response::convert_openai_chat_completions_response_to_openai_responses_non_stream_checked;
pub use response::{
    OpenAIToOpenAIResponsesStream,
    convert_openai_chat_completions_response_to_openai_responses_non_stream,
};
