//! OpenAI Chat Completions clients talking to an OpenAI Chat Completions
//! upstream: requests and responses pass through, with the model replaced.

mod request;
mod response;

pub use request::convert_openai_request_to_openai;
pub use response::{OpenAIToOpenAIStream, convert_openai_response_to_openai_non_stream};
