//! Translators for a Gemini Interactions upstream (upstream's
//! `internal/translator/interactions`), named after the client's format:
//! `claude` for Claude Messages clients.
//!
//! The other formats' translators to and from Interactions sit with the
//! format: `claude::interactions`, `codex::interactions`,
//! `gemini::interactions` and `openai::interactions`.

pub mod claude;
