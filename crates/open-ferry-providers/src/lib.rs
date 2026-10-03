//! Provider executors and OAuth logins.
//!
//! Each provider has a module with its OAuth login and refresh, and an
//! executor that implements [`open_ferry_core::executor::ProviderExecutor`].
//! [`oauth`] holds what the logins share.

pub mod claude;
pub mod codex;
pub mod oauth;
