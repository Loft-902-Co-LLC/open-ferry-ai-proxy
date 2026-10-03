//! Claude (Anthropic): OAuth login and refresh, and the executor.
//!
//! [`oauth::login`] signs in to a Claude account with Anthropic's OAuth flow
//! (PKCE, a callback on `localhost:54545`) and gives back a credential of
//! type `claude`. [`ClaudeExecutor`] calls Anthropic's Messages API with
//! such a credential or an API key, translating from and to the client's
//! format, and renews OAuth tokens.
//!
//! The executor keeps upstream's changes that make a request valid for
//! Claude: thinking settings, `max_tokens`, forced tool choice, sampling
//! settings, cache breakpoints and their TTL order, body `betas`, replayed
//! thinking signatures, and the betas a request needs. It identifies as
//! `open-ferry/<version>` unless the client sent its own `User-Agent`.
//!
//! Deviations from upstream:
//! - Nothing that makes a request look like it came from Claude Code is
//!   ported. Upstream applies a Claude Code profile to OAuth tokens and
//!   configured credentials: system-prompt injection, a synthetic
//!   `metadata.user_id`, CCH signing, `x-app`, `x-stainless-*` and
//!   `claude-cli` headers, a fixed beta list, device profiles and IDs
//!   (`claude_device_ids` is kept in files and never read), session IDs,
//!   MCP tool renaming, model-ID disguises, diagnostics, continuity tags,
//!   context management and uTLS fingerprints. Here a request carries only
//!   what the client sent plus documented headers. Anthropic may reject an
//!   OAuth request without that profile; its error is passed back.
//! - Detecting Claude Code itself isn't ported either, so every client is
//!   handled as upstream handles one it doesn't recognise.
//! - Device and account lookups before a request (`PrepareRequestAuth`),
//!   raw HTTP passthrough (`PrepareRequest`, `HttpRequest`), the Kimi
//!   thinking replay, mid-system message rebuilding and checks, payload
//!   config rules, usage reporting and request logging aren't ported.
//! - Deferred: local token counting for credentials that don't go to
//!   Anthropic's API (a 501 for now).

mod client;
mod executor;
mod headers;
mod json;
pub mod oauth;
mod ratelimit;
mod request;
mod stream;
mod thinking;
pub mod token;
mod usage;

pub use executor::ClaudeExecutor;
