//! Gemini `generateContent` clients talking to an OpenAI Chat Completions
//! upstream.

mod request;
mod response;

pub use request::convert_gemini_request_to_openai;
pub use response::{
    OpenAIToGeminiStream, convert_openai_response_to_gemini_non_stream, gemini_token_count,
};
