//! Request and response translation between OpenAI (Chat Completions, Responses),
//! Anthropic (Messages) and Gemini formats.
//!
//! Ported from CLIProxyAPI `internal/translator` and `sdk/translator` (v8.0.10, MIT).
//! <https://github.com/router-for-me/CLIProxyAPI>
//!
//! Modules are named after upstream's layout: `codex::claude` converts between
//! a Claude-format client and a Codex upstream, and `claude::openai` between
//! an OpenAI-format client and a Claude upstream. `openai::responses` converts
//! a Responses client's request for an OpenAI Chat Completions upstream
//! (upstream's `openai/openai/responses`).

mod apply_patch;
pub mod claude;
pub mod codex;
mod common;
pub mod completions;
pub mod go;
mod json;
pub mod models;
pub mod openai;
mod protowire;
pub mod registry;
mod responses_tools;
mod schema;
pub mod signature;
pub mod thinking;
