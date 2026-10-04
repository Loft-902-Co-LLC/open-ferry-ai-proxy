//! Translators between OpenAI formats and Gemini Interactions (upstream's
//! `internal/translator/openai/interactions`), both ways: `chat_completions`
//! for Chat Completions, and `responses` for OpenAI Responses.

pub mod chat_completions;
pub mod responses;
