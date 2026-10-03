//! Claude Messages clients talking to a Gemini upstream.

mod request;
mod response;

pub use request::{convert_claude_request_to_gemini, convert_claude_request_to_gemini_with_compat};
pub use response::{
    GeminiToClaudeStream, claude_token_count, convert_gemini_response_to_claude_non_stream,
};
