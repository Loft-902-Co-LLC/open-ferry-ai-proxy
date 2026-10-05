//! Provider executors and OAuth logins.
//!
//! Each provider has a module with its OAuth login and refresh, and an
//! executor that implements [`open_ferry_core::executor::ProviderExecutor`].
//! [`oauth`] holds what the logins share, and [`credentials`] saves the
//! credentials they get. [`openai_compat`] calls the OpenAI-compatible
//! providers of the config, with API keys and no login.
//! [`gemini`] calls Gemini with API keys, and Vertex AI with API keys or
//! service accounts. [`meta`] calls Meta's API with API keys.
//! [`payload`] applies the config's payload rules to the bodies the
//! executors send.

#[cfg(test)]
mod apply_patch_bridge_tests;
pub mod apply_patch_responses;
pub mod claude;
mod claude_code_session;
pub mod codex;
pub mod credentials;
mod custom_headers;
pub mod gemini;
mod go_json;
mod json;
pub mod meta;
pub mod oauth;
mod observe_send;
pub mod openai_compat;
pub mod payload;
mod redirect;
#[cfg(test)]
mod secret_echo;
mod thinking;
pub mod xai;

pub use custom_headers::is_identity_header;
pub(crate) use open_ferry_core::observe::redact;
