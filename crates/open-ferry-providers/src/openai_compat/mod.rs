//! OpenAI-compatible providers: any upstream that speaks OpenAI Chat
//! Completions, configured under `openai-compatibility`.
//!
//! Each enabled entry's API keys become credentials of provider
//! `openai-compatible-<name>`, with the entry's `base-url` and models, and
//! [`OpenAiCompatExecutor`] calls them: it translates the client's request
//! to Chat Completions (OpenAI Responses for `responses/compact`), applies
//! the thinking setting of a model suffix such as `gpt-5(high)` or of the
//! request ([`thinking`]), adjusts it as the entry's models say, and
//! translates the answer back, chunk by chunk when streaming. Token counts
//! are made locally with `tiktoken-rs`.
//!
//! Deviations from upstream (each module lists its own):
//! - Requests say `User-Agent: open-ferry/<version>`, unless the client sent
//!   its own `User-Agent`, which passes through; upstream always sends
//!   `cli-proxy-openai-compat`. A credential's `header:` attributes (the
//!   entry's `headers`) can't set `User-Agent` or another header that says
//!   which client is calling.
//! - No `prompt_cache_key` is made up: one the client sent passes through
//!   to a provider with `support-prompt-cache-key`, but upstream's derived
//!   keys (from a Claude Code prompt or a session) aren't.
//! - Not ported: the OpenAI Images endpoints (`openai-image` requests),
//!   which have no routes here yet; payload rules; and the `is-compat` flag
//!   of a model, which translators don't get.
//! - The management API's `api-call` requests through a credential
//!   (`PrepareRequest`, `HttpRequest`) aren't ported.

mod executor;
mod max_tokens;
mod status;
mod stream;
pub mod thinking;
mod tokens;
mod tool_results;

pub use executor::OpenAiCompatExecutor;
