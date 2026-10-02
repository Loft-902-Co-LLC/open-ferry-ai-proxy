//! Request and response translation between OpenAI (Chat Completions, Responses),
//! Anthropic (Messages) and Gemini formats.
//!
//! Ported from CLIProxyAPI `internal/translator` and `sdk/translator` (v8.0.10, MIT).
//! https://github.com/router-for-me/CLIProxyAPI
//!
//! Modules are named after upstream's layout: `codex::claude` converts between
//! a Claude-format client and a Codex upstream.

mod claude;
pub mod codex;
mod go;
mod json;
mod schema;
mod signature;
mod thinking;
