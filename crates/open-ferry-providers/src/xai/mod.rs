// Ported from CLIProxyAPI internal/runtime/executor/xai_executor.go
// (XAIExecutor) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! xAI: Grok's Responses API, called with an API key.
//!
//! The executor translates the client's request to Codex's Responses
//! format, applies the thinking setting of a model suffix or of the request
//! (the `thinking` module), reshapes it for Grok as upstream does (the
//! `request` and `tools` modules), and translates xAI's server-sent events
//! back.
//!
//! Deviations from upstream (each module lists its own):
//! - Only API keys are served: no xAI sign-in, no Grok CLI chat proxy, and
//!   none of the Grok CLI's identity headers or user agent.
//! - No session is made up: `x-grok-conv-id` and `prompt_cache_key` are the
//!   client's own `prompt_cache_key` or absent.
//! - Image and video generation are refused with a 400.

// The executor isn't wired yet.
#[allow(dead_code)]
mod request;
#[allow(dead_code)]
mod schema;
#[allow(dead_code)]
mod thinking;
#[allow(dead_code)]
mod tools;
