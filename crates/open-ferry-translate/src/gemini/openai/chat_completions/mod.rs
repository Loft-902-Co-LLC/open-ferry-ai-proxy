//! OpenAI Chat Completions clients talking to a Gemini upstream.

mod request;
mod response;

pub use request::convert_openai_request_to_gemini;
pub use response::{GeminiToOpenAIStream, convert_gemini_response_to_openai_non_stream};
