//! Gemini `generateContent` clients talking to a Claude upstream.

mod request;
mod response;

pub use request::convert_gemini_request_to_claude;
pub use response::{
    ClaudeToGeminiStream, convert_claude_response_to_gemini_non_stream, gemini_token_count,
};
