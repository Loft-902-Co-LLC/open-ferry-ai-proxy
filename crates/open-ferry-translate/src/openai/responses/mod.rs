//! OpenAI Responses clients talking to an OpenAI Chat Completions upstream.
//!
//! Only the request is ported so far. Upstream's handler for
//! `/v1/chat/completions` also calls it directly, for a client that sends a
//! Responses body there: one with `input` or `instructions` and no
//! `messages`.

mod request;
mod tool_index;
mod tools;

pub use request::convert_openai_responses_request_to_openai_chat_completions;
