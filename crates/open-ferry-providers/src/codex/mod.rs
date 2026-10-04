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
//! `responses/compact` calls go to Codex's compact endpoint. A client on the
//! Responses WebSocket, with a credential that has `websockets` on, calls
//! Codex over a WebSocket too (the `websocket` module). Token counts
//! are made locally with `tiktoken-rs`. For Claude clients, which drop
//! Codex's reasoning items, each turn's reasoning and tool calls are kept
//! by the session the client named and put back in its next request (the
//! `replay` module). Codex Alpha Search payloads go out untranslated, as
//! the executor's plain HTTP requests.
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
//! - The config's image generation switch isn't ported, and its payload
//!   rules are left to [`crate::payload`]. Compatibility models and Codex
//!   clients' multi-agent v2 and orphan delegation requests are handled in
//!   the `compat` module.
//!
//! Deferred:
//! - Image generation: the `image_generation` tool upstream adds, and the
//!   OpenAI Images endpoints served through Codex.

pub(crate) mod claude_tokens;
pub(crate) mod client;
pub(crate) mod compat;
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
mod websocket;

pub use client::USER_AGENT;
pub use executor::CodexExecutor;
