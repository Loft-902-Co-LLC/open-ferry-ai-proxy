//! Provider executors and OAuth logins.
//!
//! Each provider has a module with its OAuth login and refresh, and an
//! executor that implements [`open_ferry_core::executor::ProviderExecutor`].
//! [`oauth`] holds what the logins share. [`openai_compat`] calls the
//! OpenAI-compatible providers of the config, with API keys and no login.

pub mod claude;
pub mod codex;
mod custom_headers;
mod go_json;
mod json;
pub mod oauth;
pub mod openai_compat;
mod redact;
