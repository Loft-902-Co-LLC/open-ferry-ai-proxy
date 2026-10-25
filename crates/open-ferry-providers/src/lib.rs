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
//! executors send. The `images` module holds what the executors that call
//! an Image API (`/images/generations` and `/images/edits`) share.
//! [`claude_cli`] serves Claude models by running the user's own installed
//! Claude Code, which signs itself in; open-ferry sends nothing to
//! Anthropic for it.

#[cfg(test)]
mod apply_patch_bridge_tests;
#[cfg(test)]
mod apply_patch_integration_tests;
pub mod apply_patch_responses;
pub mod claude;
pub mod claude_cli;
mod claude_code_session;
pub mod codex;
pub mod credentials;
mod custom_headers;
pub mod gemini;
mod go_json;
mod images;
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
mod transport;
#[cfg(test)]
mod unsupported_part_tests;
pub mod xai;

pub use custom_headers::is_identity_header;

/// Scratch storage for this crate's tests: under `OFP_TEST_SCRATCH` when
/// set (CI's Windows runner), else under the workspace's `target/ofp-scratch`.
#[cfg(test)]
pub(crate) fn test_scratch_dir() -> tempfile::TempDir {
    let root = std::env::var_os("OFP_TEST_SCRATCH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/ofp-scratch")
        });
    let root = open_ferry_core::auth::path::clean(&root);
    std::fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}
pub(crate) use open_ferry_core::observe::redact;
