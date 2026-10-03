//! Codex: ChatGPT's Responses backend, signed in with OpenAI's OAuth.
//!
//! [`oauth`] signs in and refreshes tokens; [`CodexExecutor`] calls Codex
//! with a credential of provider `codex`: a ChatGPT sign-in, or an API key
//! in the `api_key` attribute with an optional `base_url`.
//!
//! The executor translates the client's request to Codex's Responses
//! format, applies the thinking setting of a model suffix such as
//! `gpt-5(high)` or of the request ([`thinking`]), adjusts it as upstream
//! does, and translates Codex's server-sent events back, chunk by chunk
//! when streaming and from the completed response otherwise.
//! `responses/compact` calls go to Codex's compact endpoint. Token counts
//! are made locally with `tiktoken-rs`. For Claude clients, which drop
//! Codex's reasoning items, each turn's reasoning and tool calls are kept
//! by the session the client named and put back in its next request (the
//! `replay` module).
//!
//! Deviations from upstream (each module lists its own):
//! - Our requests don't pass for Codex's own client: there is no
//!   `codex_cli_rs` or `codex-tui` `User-Agent`, no made-up `Originator`,
//!   session ID, conversation ID, prompt cache key or routing hint, and no
//!   cloaking, TLS fingerprinting or `override_header` from models.json.
//!   They say `User-Agent: open-ferry/<version>`, unless the client sent its
//!   own `User-Agent`, which passes through as upstream passes it. Headers
//!   and a `prompt_cache_key` the client itself sent pass through where
//!   upstream passes them. If Codex then rejects a request, its error is
//!   passed back as it is.
//! - Config-driven behaviour isn't ported: payload rules, compat models (so
//!   reasoning items are always cleaned for GPT), and the image generation
//!   switch.
//!
//! Deferred:
//! - The Responses WebSocket upstream (`codex_websockets_executor.go`),
//!   which is a separate transport.
//! - Image generation: the `image_generation` tool upstream adds, and the
//!   OpenAI Images endpoints served through Codex.

pub(crate) mod claude_tokens;
pub(crate) mod client;
mod executor;
mod ext;
mod input_ids;
pub mod jwt;
pub mod oauth;
pub(crate) mod reasoning;
mod replay;
mod replay_cache;
pub(crate) mod request;
pub(crate) mod stream;
pub(crate) mod terminal;
pub mod thinking;
pub mod token;
mod tokens;
mod tool_schema;
pub(crate) mod usage;

pub use client::USER_AGENT;
pub use executor::CodexExecutor;
