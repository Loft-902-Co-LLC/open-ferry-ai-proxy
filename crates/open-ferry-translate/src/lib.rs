//! Request and response translation between OpenAI (Chat Completions, Responses),
//! Anthropic (Messages) and Gemini formats.
//!
//! Ported from CLIProxyAPI `internal/translator` and `sdk/translator` (v8.0.10, MIT).
//! <https://github.com/router-for-me/CLIProxyAPI>
//!
//! Modules are named after upstream's layout: `codex::claude` converts between
//! a Claude-format client and a Codex upstream, and `claude::openai` between
//! an OpenAI-format client and a Claude upstream. `openai::claude` and
//! `openai::responses` convert between Claude and Responses clients and an
//! OpenAI Chat Completions upstream (upstream's `openai/claude` and
//! `openai/openai/responses`), and `openai::chat_completions` passes Chat
//! Completions through (upstream's `openai/openai/chat-completions`).

pub mod apply_patch;
pub mod claude;
pub mod codex;
pub mod codex_client;
mod common;
pub mod completions;
pub mod gemini;
mod gemini_schema;
pub mod go;
pub mod interactions;
mod json;
pub mod models;
pub mod openai;
mod protowire;
pub mod registry;
mod responses_tools;
mod schema;
pub mod signature;
pub mod thinking;
