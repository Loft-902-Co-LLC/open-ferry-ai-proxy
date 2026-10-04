// Ported from CLIProxyAPI internal/runtime/executor/xai_executor.go
// (XAIExecutor) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! xAI: Grok's Responses API, called with an API key.
//!
//! The executor translates the client's request to Codex's Responses
//! format, applies the thinking setting of a model suffix or of the request
//! (the `thinking` module), reshapes it for Grok as upstream does (the
//! `request`, `tools` and `reasoning` modules), and translates xAI's
//! server-sent events back, undoing the reshaping and hiding X search's own
//! tool calls (the `response` and `reasoning` modules). A non-streaming call
//! reads xAI's stream to its terminal event, a streaming one translates it
//! as it comes (the `stream` module), and `responses/compact` or a
//! `compaction_trigger` item goes to `/responses/compact` (the `compact`
//! module). Error statuses keep upstream's remapping (the `errors` module).
//! Reasoning replay is a hook that does nothing yet (the `replay` module).
//! Token counts are estimated locally with `o200k_base` (the `tokens`
//! module).
//!
//! Deviations from upstream (each module lists its own):
//! - Only API keys are served: no xAI sign-in, no Grok CLI chat proxy, and
//!   none of the Grok CLI's identity headers or user agent.
//! - No session is made up: `x-grok-conv-id` and `prompt_cache_key` are the
//!   client's own `prompt_cache_key` or absent.
//! - Image and video generation are refused with a 400.
//! - HTTP only: upstream's WebSocket executor and reasoning replay aren't
//!   ported yet.

mod compact;
mod errors;
mod executor;
mod reasoning;
mod replay;
mod request;
mod response;
mod schema;
mod stream;
mod thinking;
mod tokens;
mod tools;

pub use executor::XaiExecutor;
