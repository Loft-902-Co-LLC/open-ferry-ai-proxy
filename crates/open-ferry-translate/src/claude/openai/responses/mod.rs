//! OpenAI Responses clients talking to a Claude upstream.

mod request;
mod response;
mod tools;
mod web_search;

pub use request::{
    convert_openai_responses_request_to_claude,
    convert_openai_responses_request_to_claude_with_compat,
};
pub(crate) use response::convert_claude_response_to_openai_responses_non_stream_checked;
pub use response::{
    ClaudeToOpenAIResponsesStream, convert_claude_response_to_openai_responses_non_stream,
};

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
