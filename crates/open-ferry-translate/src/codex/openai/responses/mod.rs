//! OpenAI Responses clients talking to a Codex upstream.

mod request;
mod response;

pub use request::convert_openai_responses_request_to_codex;
pub use response::{
    CodexToOpenAIResponsesStream, convert_codex_response_to_openai_responses_non_stream,
    convert_codex_response_to_openai_responses_non_stream_with_bridge,
};
