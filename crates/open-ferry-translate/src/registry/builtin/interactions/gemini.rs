// Ported from CLIProxyAPI internal/translator/gemini/interactions/init.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Gemini and Interactions translators' registrations: Interactions
//! clients to a Gemini upstream (`interactions` → `gemini`), Gemini clients
//! to an Interactions upstream (`gemini` → `interactions`), and Interactions
//! passed through (`interactions` → `interactions`).
//!
//! Deviations from upstream: none.

use std::sync::Arc;

use super::super::{non_empty, to_vec};
use crate::gemini::interactions as gemini_interactions;
use crate::registry::{Format, Registry, ResponseTransform, StreamTranslator};

pub(super) fn register(registry: &Registry) {
    registry.register(
        Format::INTERACTIONS,
        Format::INTERACTIONS,
        Some(Arc::new(|model, body, stream| {
            gemini_interactions::convert_interactions_request_to_interactions(model, body, stream)
        })),
        ResponseTransform {
            stream: Some(Arc::new(|_context| Box::new(InteractionsPassthrough))),
            non_stream: Some(Arc::new(|_context, body| {
                Some(
                    gemini_interactions::convert_interactions_response_passthrough_non_stream(body)
                        .to_vec(),
                )
            })),
            token_count: None,
        },
    );

    registry.register(
        Format::INTERACTIONS,
        Format::GEMINI,
        Some(Arc::new(|model, body, stream| {
            gemini_interactions::convert_interactions_request_to_gemini(model, &body, stream)
        })),
        ResponseTransform {
            stream: Some(Arc::new(|context| {
                Box::new(GeminiToInteractions(
                    gemini_interactions::GeminiToInteractionsStream::new(context.model),
                ))
            })),
            non_stream: Some(Arc::new(|context, body| {
                let response =
                    gemini_interactions::convert_gemini_response_to_interactions_non_stream(
                        context.model,
                        body,
                    );
                Some(to_vec(&response))
            })),
            token_count: None,
        },
    );

    registry.register(
        Format::GEMINI,
        Format::INTERACTIONS,
        Some(Arc::new(|model, body, stream| {
            gemini_interactions::convert_gemini_request_to_interactions(model, &body, stream)
        })),
        ResponseTransform {
            stream: Some(Arc::new(|context| {
                Box::new(InteractionsToGemini(
                    gemini_interactions::InteractionsToGeminiStream::new(context.model),
                ))
            })),
            non_stream: Some(Arc::new(|context, body| {
                let response =
                    gemini_interactions::convert_interactions_response_to_gemini_non_stream(
                        context.model,
                        body,
                    );
                Some(to_vec(&response))
            })),
            token_count: None,
        },
    );
}

/// Interactions → Interactions: each chunk passes through.
struct InteractionsPassthrough;

impl StreamTranslator for InteractionsPassthrough {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        gemini_interactions::convert_interactions_response_passthrough(chunk)
            .map(|chunk| non_empty(chunk.to_vec()))
            .unwrap_or_default()
    }
}

/// Gemini → Interactions: an SSE frame for each event a chunk gives.
struct GeminiToInteractions(gemini_interactions::GeminiToInteractionsStream);

impl StreamTranslator for GeminiToInteractions {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.0
            .translate(chunk)
            .into_iter()
            .map(String::into_bytes)
            .collect()
    }
}

/// Interactions → Gemini: a Gemini chunk for each event that makes one.
struct InteractionsToGemini(gemini_interactions::InteractionsToGeminiStream);

impl StreamTranslator for InteractionsToGemini {
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.0.translate(chunk).iter().map(to_vec).collect()
    }
}
