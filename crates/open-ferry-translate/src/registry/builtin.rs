//! The built-in translators, registered as upstream's `init` functions
//! register them, and the adapters between their APIs and the registry's.

use std::error::Error;
use std::sync::Arc;

use serde_json::Value;

use super::{Format, Registry, ResponseTransform, StreamTranslator};
use crate::claude::openai::{chat_completions as claude_chat, responses as claude_responses};
use crate::codex::claude as codex_claude;
use crate::codex::openai::{chat_completions as codex_chat, responses as codex_responses};
use crate::json::raw;
use crate::json::str_of;

pub(super) fn register(registry: &Registry) {
    let models = registry.models;

    // internal/translator/codex/claude/init.go
    registry.register(
        Format::CLAUDE,
        Format::CODEX,
        Some(Arc::new(|model, body, _stream| {
            codex_claude::convert_claude_request_to_codex(model, &body)
        })),
        ResponseTransform {
            stream: Some(Arc::new(|context| {
                Box::new(CodexToClaude(codex_claude::CodexToClaudeStream::new(
                    context.original_request,
                )))
            })),
            non_stream: Some(Arc::new(|context, body| {
                let event = parse(body);
                let response = codex_claude::convert_codex_response_to_claude_non_stream(
                    context.original_request,
                    &event,
                );
                Some(response.as_ref().map(to_vec).unwrap_or_default())
            })),
            token_count: Some(Arc::new(|count| {
                to_vec(&codex_claude::claude_token_count(count))
            })),
        },
    );

    // internal/translator/codex/openai/chat-completions/init.go
    registry.register(
        Format::OPENAI,
        Format::CODEX,
        Some(Arc::new(|model, body, stream| {
            codex_chat::convert_openai_chat_completions_request_to_codex(model, &body, stream)
        })),
        ResponseTransform {
            stream: Some(Arc::new(|context| {
                Box::new(CodexToChat(
                    codex_chat::CodexToOpenAIChatCompletionsStream::new(
                        context.model,
                        context.original_request,
                    ),
                ))
            })),
            non_stream: Some(Arc::new(|context, body| {
                let event = parse(body);
                let response =
                    codex_chat::convert_codex_response_to_openai_chat_completions_non_stream(
                        context.original_request,
                        &event,
                    );
                Some(response.as_ref().map(to_vec).unwrap_or_default())
            })),
            token_count: None,
        },
    );

    // internal/translator/codex/openai/responses/init.go
    registry.register(
        Format::OPENAI_RESPONSE,
        Format::CODEX,
        Some(Arc::new(|model, body, _stream| {
            codex_responses::convert_openai_responses_request_to_codex(model, body)
        })),
        ResponseTransform {
            stream: Some(Arc::new(|context| {
                Box::new(CodexToResponses(
                    codex_responses::CodexToOpenAIResponsesStream::new(
                        context.model,
                        context.original_request,
                        context.request,
                    ),
                ))
            })),
            non_stream: Some(Arc::new(|_context, body| {
                Some(codex_responses_non_stream(body))
            })),
            token_count: None,
        },
    );

    // internal/translator/claude/openai/chat-completions/init.go
    registry.register(
        Format::OPENAI,
        Format::CLAUDE,
        Some(Arc::new(move |model, body, stream| {
            claude_chat::convert_openai_chat_completions_request_to_claude(
                model, &body, stream, models,
            )
        })),
        ResponseTransform {
            stream: Some(Arc::new(|context| {
                Box::new(ClaudeToChat(
                    claude_chat::ClaudeToOpenAIChatCompletionsStream::new(context.model),
                ))
            })),
            non_stream: Some(Arc::new(|_context, body| {
                let response =
                    claude_chat::convert_claude_response_to_openai_chat_completions_non_stream(
                        body,
                    );
                Some(to_vec(&response))
            })),
            token_count: None,
        },
    );

    // internal/translator/claude/openai/responses/init.go
    registry.register(
        Format::OPENAI_RESPONSE,
        Format::CLAUDE,
        Some(Arc::new(move |model, body, stream| {
            claude_responses::convert_openai_responses_request_to_claude(
                model, &body, stream, models,
            )
        })),
        ResponseTransform {
            stream: Some(Arc::new(|context| {
                Box::new(ClaudeToResponses(
                    claude_responses::ClaudeToOpenAIResponsesStream::new(
                        context.model,
                        context.original_request,
                        context.request,
                    ),
                ))
            })),
            non_stream: Some(Arc::new(|context, body| {
                claude_responses::convert_claude_response_to_openai_responses_non_stream_checked(
                    context.original_request,
                    context.request,
                    body,
                )
                .as_ref()
                .map(to_vec)
            })),
            token_count: None,
        },
    );
}

fn to_vec(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("a JSON value always serializes")
}

/// A provider's response body. One that isn't valid JSON reads as an event
/// with no fields.
fn parse(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or(Value::Null)
}

/// `ConvertCodexResponseToOpenAIResponsesNonStream`, on the body's text so
/// the response passes through as Codex wrote it: the `response` of a
/// `response.completed` or `response.incomplete` event, or a body that is
/// already a response (no `type`, with an `output` list) whole. Anything else
/// gives an empty body.
fn codex_responses_non_stream(body: &[u8]) -> Vec<u8> {
    let event = parse(body);
    let kind = str_of(event.get("type"));
    if kind.is_empty() && event.get("output").is_some_and(Value::is_array) {
        return body.to_vec();
    }
    if kind != "response.completed" && kind != "response.incomplete" {
        return Vec::new();
    }
    // The body parsed, so it is valid UTF-8 and valid JSON.
    let text = std::str::from_utf8(body).expect("parsed JSON is UTF-8");
    raw::member(text, "response")
        .map(|response| response.as_bytes().to_vec())
        .unwrap_or_default()
}

/// Codex → Claude Messages. Upstream returns one chunk for each `data:` line,
/// holding all the events it gives.
struct CodexToClaude(codex_claude::CodexToClaudeStream);

impl StreamTranslator for CodexToClaude {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        non_empty(self.0.translate_line(chunk).into_bytes())
    }
}

/// Codex → Chat Completions: at most one chunk per line.
struct CodexToChat(codex_chat::CodexToOpenAIChatCompletionsStream);

impl StreamTranslator for CodexToChat {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.0
            .translate_line(chunk)
            .as_ref()
            .map(to_vec)
            .into_iter()
            .collect()
    }
}

/// Codex → Responses: every line passes through, sometimes with the model
/// filled in.
struct CodexToResponses(codex_responses::CodexToOpenAIResponsesStream);

impl StreamTranslator for CodexToResponses {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        non_empty(self.0.translate_line(chunk).into_owned())
    }
}

/// Claude Messages → Chat Completions: at most one chunk per line.
struct ClaudeToChat(claude_chat::ClaudeToOpenAIChatCompletionsStream);

impl StreamTranslator for ClaudeToChat {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.0
            .translate_line(chunk)
            .as_ref()
            .map(to_vec)
            .into_iter()
            .collect()
    }
}

/// Claude Messages → Responses. Upstream returns one chunk per event.
struct ClaudeToResponses(claude_responses::ClaudeToOpenAIResponsesStream);

impl StreamTranslator for ClaudeToResponses {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        events(&self.0.translate_line(chunk))
    }

    fn finish(&mut self) -> Vec<Vec<u8>> {
        events(&self.0.finalize_tool_input())
    }

    fn tool_input_error(&self) -> Option<&(dyn Error + 'static)> {
        self.0.tool_input_error()
    }
}

fn non_empty(chunk: Vec<u8>) -> Vec<Vec<u8>> {
    if chunk.is_empty() {
        Vec::new()
    } else {
        vec![chunk]
    }
}

/// Splits SSE frames, each ending in a blank line, into one chunk each.
fn events(frames: &str) -> Vec<Vec<u8>> {
    frames
        .split_inclusive("\n\n")
        .map(|frame| frame.as_bytes().to_vec())
        .collect()
}
