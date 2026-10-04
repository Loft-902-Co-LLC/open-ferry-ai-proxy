//! The config's payload rules applied to the bodies sent upstream:
//! defaults, overrides and filters, by model, protocol, request headers and
//! conditions on the body (upstream's internal/runtime/executor/helps/
//! payload_helpers.go and payload_mutations.go). Not ported yet (P3 WP-D).
//!
//! What is here is the hook the executors call, with the signature the
//! port keeps: each executor calls [`apply`] once its body is translated,
//! where upstream calls `ApplyPayloadConfigWithTrackedPathsForExecutor` or
//! one of its wrappers. For now it leaves the body as it is and reports no
//! path touched; the Codex clients' integer pass upstream runs first in it
//! is still the executors' own call to the Codex `compat` module.
//!
//! Deviations from upstream: no rule is applied yet. Every executor names
//! itself in [`Target::executor`]; upstream names only the Codex ones, the
//! only names it checks.

mod matchers;
mod path;
mod query;
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;

use open_ferry_core::config::Config;
use open_ferry_core::exec::{Format, Options, Request};
use serde_json::Value;

/// Where a body goes: what the rules match against besides the request.
#[derive(Clone, Copy, Debug)]
pub struct Target<'a> {
    /// The executor's identifier, such as `claude`, `codex` or
    /// `codex-websockets` (upstream's `targetExecutor`).
    pub executor: &'a str,
    /// The format the body is in (upstream's `protocol`).
    pub protocol: &'a Format,
    /// The model sent upstream, without a thinking suffix.
    pub model: &'a str,
    /// The path the rules' paths are under; empty for the body's root.
    pub root: &'a str,
    /// Whether the body asks for a stream, as the client's original
    /// request is translated for the defaults' checks.
    pub stream: bool,
    /// The paths the caller wants to know an applied rule wrote or deleted
    /// (upstream's `trackedPaths`).
    pub tracked: &'a [&'a str],
}

/// The tracked paths an applied rule wrote or deleted, or wrote or deleted
/// something under.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Touched(BTreeSet<String>);

impl Touched {
    /// Whether a rule touched the tracked `path`.
    pub fn contains(&self, path: &str) -> bool {
        self.0.contains(path)
    }

    /// Whether no rule touched a tracked path.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Applies `config`'s payload rules to `body`, the translated `request`
/// made with `options`, going to `target`, and says which tracked paths
/// they touched (upstream's `ApplyPayloadConfigWithTrackedPathsForExecutor`).
/// For now, changes nothing.
pub fn apply(
    config: Option<&Config>,
    target: &Target<'_>,
    request: &Request,
    options: &Options,
    body: &mut Value,
) -> Touched {
    let _ = (config, target, request, options, body);
    Touched::default()
}
