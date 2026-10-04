//! Translators for a Gemini upstream (upstream's `internal/translator/gemini`),
//! named after the client's format: `claude` for Claude Messages clients,
//! `openai` for OpenAI clients and `gemini` for Gemini clients, whose
//! requests are only normalized.

pub mod claude;
pub(crate) mod common;
#[allow(
    clippy::module_inception,
    reason = "named after the client's format, as the other modules here are"
)]
pub mod gemini;
pub mod interactions;
pub mod openai;
