// Ported from CLIProxyAPI internal/translator/gemini/gemini/init.go,
// internal/translator/gemini/claude/init.go and
// internal/translator/gemini/openai/chat-completions/init.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The translators to a Gemini upstream.
//!
//! Deviations from upstream: none.

use std::sync::Arc;

use super::{checked, non_empty, parse, sends, to_vec};
use crate::gemini::claude as gemini_claude;
use crate::gemini::gemini as gemini_gemini;
use crate::gemini::openai::chat_completions as gemini_chat;
use crate::models::ModelCatalog;
use crate::registry::{Format, Registry, ResponseTransform, StreamTranslator};

pub(super) fn register(registry: &Registry) {
    // internal/translator/gemini/gemini/init.go
    registry.register(
        Format::GEMINI,
        Format::GEMINI,
        sends(|model, body, stream| {
            gemini_gemini::convert_gemini_request_to_gemini(model, body, stream)
        }),
        ResponseTransform {
            stream: Some(Arc::new(|_context| Box::new(GeminiToGemini))),
            non_stream: Some(Arc::new(|_context, body| {
                Some(gemini_gemini::passthrough_gemini_response_non_stream(body).to_vec())
            })),
            token_count: Some(Arc::new(|count| {
                to_vec(&gemini_gemini::gemini_token_count(count))
            })),
        },
    );

    // internal/translator/gemini/claude/init.go
    registry.register(
        Format::CLAUDE,
        Format::GEMINI,
        checked(|model, body, stream| {
            gemini_claude::convert_claude_request_to_gemini(
                model,
                &body,
                stream,
                &ModelCatalog::current(),
            )
        }),
        ResponseTransform {
            stream: Some(Arc::new(|context| {
                Box::new(GeminiToClaude(gemini_claude::GeminiToClaudeStream::new(
                    context.original_request,
                )))
            })),
            non_stream: Some(Arc::new(|context, body| {
                let response = gemini_claude::convert_gemini_response_to_claude_non_stream(
                    context.original_request,
                    &parse(body),
                );
                Some(to_vec(&response))
            })),
            token_count: Some(Arc::new(|count| {
                to_vec(&gemini_claude::claude_token_count(count))
            })),
        },
    );

    // internal/translator/gemini/openai/chat-completions/init.go
    registry.register(
        Format::OPENAI,
        Format::GEMINI,
        checked(|model, body, stream| {
            gemini_chat::convert_openai_request_to_gemini(model, &body, stream)
        }),
        ResponseTransform {
            stream: Some(Arc::new(|context| {
                Box::new(GeminiToChat(gemini_chat::GeminiToOpenAIStream::new(
                    context.original_request,
                )))
            })),
            non_stream: Some(Arc::new(|context, body| {
                let response = gemini_chat::convert_gemini_response_to_openai_non_stream(
                    context.original_request,
                    &parse(body),
                );
                Some(to_vec(&response))
            })),
            token_count: None,
        },
    );
}

/// Gemini → Gemini: each line's payload passes through.
struct GeminiToGemini;

impl StreamTranslator for GeminiToGemini {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        gemini_gemini::passthrough_gemini_response_stream(chunk)
            .map(|chunk| non_empty(chunk.to_vec()))
            .unwrap_or_default()
    }
}

/// Gemini → Claude Messages. Upstream returns one chunk per Gemini chunk,
/// holding all the events it gives.
struct GeminiToClaude(gemini_claude::GeminiToClaudeStream);

impl StreamTranslator for GeminiToClaude {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        non_empty(self.0.translate(chunk).into_bytes())
    }
}

/// Gemini → Chat Completions: one chunk per candidate.
struct GeminiToChat(gemini_chat::GeminiToOpenAIStream);

impl StreamTranslator for GeminiToChat {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.0.translate(chunk).iter().map(to_vec).collect()
    }
}
