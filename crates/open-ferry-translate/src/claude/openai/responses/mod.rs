//! OpenAI Responses clients talking to a Claude upstream.

mod request;
mod response;
mod tool_search;
mod tools;
mod web_search;

pub use request::{
    convert_openai_responses_request_to_claude,
    convert_openai_responses_request_to_claude_with_compat,
    convert_openai_responses_request_to_claude_with_tool_search,
};
pub(crate) use response::convert_claude_response_to_openai_responses_non_stream_checked;
pub use response::{
    ClaudeToOpenAIResponsesStream, convert_claude_response_to_openai_responses_non_stream,
    convert_claude_response_to_openai_responses_non_stream_with_tool_search,
};

/// The Responses → Claude pair with the search setting retained by its
/// request and response transforms. A per-request registry avoids process
/// globals and preserves the registry's reasoning-summary and refusal policy.
pub fn registry_with_tool_search(enabled: bool) -> crate::registry::Registry {
    use crate::registry::{Format, Registry, ResponseTransform};
    use std::sync::Arc;

    let registry = Registry::new();
    registry.register(
        Format::OPENAI_RESPONSE,
        Format::CLAUDE,
        Some(Arc::new(move |model, body, stream| {
            convert_openai_responses_request_to_claude_with_tool_search(
                model,
                &body,
                stream,
                &crate::models::ModelCatalog::current(),
                false,
                enabled,
            )
        })),
        ResponseTransform {
            stream: Some(Arc::new(move |context| {
                Box::new(ClaudeToOpenAIResponsesStream::with_tool_search(
                    context.model,
                    context.original_request,
                    context.request,
                    enabled,
                ))
            })),
            non_stream: Some(Arc::new(move |context, body| {
                convert_claude_response_to_openai_responses_non_stream_with_tool_search(
                    context.original_request,
                    context.request,
                    body,
                    enabled,
                )
                .map(|value| value.to_string().into_bytes())
            })),
            token_count: None,
        },
    );
    registry
}

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod tool_search_tests;
