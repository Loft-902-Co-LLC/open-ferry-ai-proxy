//! The translators for Gemini `generateContent` clients, registered as
//! upstream's `init` functions in `internal/translator/{codex,claude,openai}/gemini`
//! register them.

use std::sync::Arc;

use serde_json::Value;

use super::super::{Format, Registry, ResponseTransform, StreamTranslator};
use crate::claude::gemini as claude_gemini;
use crate::codex::gemini as codex_gemini;
use crate::models::ModelCatalog;
use crate::openai::gemini as openai_gemini;

pub(super) fn register(registry: &Registry) {
    // internal/translator/codex/gemini/init.go
    registry.register(
        Format::GEMINI,
        Format::CODEX,
        Some(Arc::new(|model, body, _stream| {
            codex_gemini::convert_gemini_request_to_codex(model, &body)
        })),
        ResponseTransform {
            stream: Some(Arc::new(|context| {
                Box::new(CodexToGemini(codex_gemini::CodexToGeminiStream::new(
                    context.model,
                    context.original_request,
                )))
            })),
            non_stream: Some(Arc::new(|context, body| {
                let event = parse(body);
                let response = codex_gemini::convert_codex_response_to_gemini_non_stream(
                    context.model,
                    context.original_request,
                    &event,
                );
                Some(response.as_ref().map(to_vec).unwrap_or_default())
            })),
            token_count: Some(Arc::new(|count| {
                to_vec(&codex_gemini::gemini_token_count(count))
            })),
        },
    );

    // internal/translator/claude/gemini/init.go
    registry.register(
        Format::GEMINI,
        Format::CLAUDE,
        Some(Arc::new(|model, body, stream| {
            claude_gemini::convert_gemini_request_to_claude(
                model,
                &body,
                stream,
                &ModelCatalog::current(),
            )
        })),
        ResponseTransform {
            stream: Some(Arc::new(|context| {
                Box::new(ClaudeToGemini(claude_gemini::ClaudeToGeminiStream::new(
                    context.model,
                )))
            })),
            non_stream: Some(Arc::new(|context, body| {
                let response = claude_gemini::convert_claude_response_to_gemini_non_stream(
                    context.model,
                    body,
                );
                Some(to_vec(&response))
            })),
            token_count: Some(Arc::new(|count| {
                to_vec(&claude_gemini::gemini_token_count(count))
            })),
        },
    );

    // internal/translator/openai/gemini/init.go
    registry.register(
        Format::GEMINI,
        Format::OPENAI,
        Some(Arc::new(|model, body, stream| {
            openai_gemini::convert_gemini_request_to_openai(model, &body, stream)
        })),
        ResponseTransform {
            stream: Some(Arc::new(|_context| {
                Box::new(OpenAIToGemini(openai_gemini::OpenAIToGeminiStream::new()))
            })),
            non_stream: Some(Arc::new(|_context, body| {
                let response = openai_gemini::convert_openai_response_to_gemini_non_stream(body);
                Some(to_vec(&response))
            })),
            token_count: Some(Arc::new(|count| {
                to_vec(&openai_gemini::gemini_token_count(count))
            })),
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

/// One chunk for each Gemini response a line gives.
fn chunks(responses: Vec<Value>) -> Vec<Vec<u8>> {
    responses.iter().map(to_vec).collect()
}

/// Codex → Gemini: one chunk per Gemini response.
struct CodexToGemini(codex_gemini::CodexToGeminiStream);

impl StreamTranslator for CodexToGemini {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        chunks(self.0.translate_line(chunk))
    }
}

/// Claude Messages → Gemini: one chunk per Gemini response.
struct ClaudeToGemini(claude_gemini::ClaudeToGeminiStream);

impl StreamTranslator for ClaudeToGemini {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        chunks(self.0.translate_line(chunk))
    }
}

/// Chat Completions → Gemini: one chunk per Gemini response.
struct OpenAIToGemini(openai_gemini::OpenAIToGeminiStream);

impl StreamTranslator for OpenAIToGemini {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        chunks(self.0.translate_line(chunk))
    }
}

#[cfg(test)]
mod tests;
