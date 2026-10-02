//! OpenAI Chat Completions clients talking to a Claude upstream.
mod request;
mod response;
pub use request::{
    convert_openai_chat_completions_request_to_claude,
    convert_openai_chat_completions_request_to_claude_with_compat,
};
pub use response::{
    ClaudeToOpenAIChatCompletionsStream,
    convert_claude_response_to_openai_chat_completions_non_stream,
};
