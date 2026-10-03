//! Gemini clients talking to a Gemini upstream: requests are normalized to
//! what the Gemini API accepts, and responses pass through.

mod request;
mod response;

pub use request::convert_gemini_request_to_gemini;
pub use response::{
    gemini_token_count, passthrough_gemini_response_non_stream, passthrough_gemini_response_stream,
};
