//! Claude Messages clients talking to an OpenAI Chat Completions upstream.

mod request;
mod response;

pub use request::{convert_claude_request_to_openai, convert_claude_request_to_openai_with_compat};
pub use response::{
    OpenAIToClaudeStream, claude_token_count, convert_openai_response_to_claude_non_stream,
};
