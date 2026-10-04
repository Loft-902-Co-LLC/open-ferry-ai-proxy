//! Config, accounts, credential store and routing.
//!
//! [`exec`] describes a call and its result, as the HTTP layer hands it to a
//! [`exec::Dispatcher`], and [`models`] which providers serve a model;
//! [`registry`] keeps that for the configured credentials.
//! Behind them, [`auth`] holds credentials and [`executor`] the trait each
//! provider's executor implements, and [`manager`] is the dispatcher: it
//! picks credentials, calls executors, and tracks cooldowns and refreshes.
//! [`config`] loads the proxy's config file and watches it and the auth
//! directory for changes. [`codex_models`] builds the model list Codex
//! clients fetch. [`observe`] is what the request log and the usage
//! statistics see of each request and its upstream calls.

pub mod auth;
pub mod codex_models;
pub mod config;
pub mod exec;
pub mod executor;
pub mod manager;
pub mod models;
pub mod observe;
pub mod registry;
