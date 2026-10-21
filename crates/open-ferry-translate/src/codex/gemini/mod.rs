//! Gemini `generateContent` clients talking to a Codex upstream.

mod request;
mod signature;
mod response;

pub use request::convert_gemini_request_to_codex;
pub use response::{
    CodexToGeminiStream, convert_codex_response_to_gemini_non_stream, gemini_token_count,
};

#[cfg(test)]
mod signature_tests;
