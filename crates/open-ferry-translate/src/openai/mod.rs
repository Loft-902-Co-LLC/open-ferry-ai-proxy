//! Translators for an OpenAI Chat Completions upstream, named after the
//! client's format: `claude` for Claude Messages clients, `responses` for
//! OpenAI Responses clients, and `chat_completions` for Chat Completions
//! clients, whose requests and responses pass through.

pub mod chat_completions;
pub mod claude;
pub mod responses;
