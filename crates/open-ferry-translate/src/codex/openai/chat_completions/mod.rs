//! OpenAI Chat Completions clients talking to a Codex upstream.

mod request;
mod response;

pub use request::convert_openai_chat_completions_request_to_codex;
pub use response::{
    CodexToOpenAIChatCompletionsStream,
    convert_codex_response_to_openai_chat_completions_non_stream,
};
