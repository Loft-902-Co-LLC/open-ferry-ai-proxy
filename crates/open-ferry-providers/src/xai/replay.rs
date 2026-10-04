// Ported from CLIProxyAPI internal/runtime/executor/xai_reasoning_replay.go
// (the hook points only: applyXAIReasoningReplayCacheRequired,
// cacheXAIReasoningReplayFromCompleted, clearXAIReasoningReplayAfterCompaction)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Where upstream replays Grok's reasoning, which does nothing yet.
//!
//! Upstream keeps the reasoning items of a session's last completed
//! response and puts them back into the next request's input, since
//! clients drop them. The executor already calls the three hooks where
//! upstream does: [`apply`] once the tools are clamped, [`cache_completed`]
//! for a `response.completed` event, and [`clear_after_compaction`] after a
//! successful compact call.
//!
//! Deviations from upstream:
//! - Not ported yet: nothing is kept or replayed, and a request is sent as
//!   it is.

use open_ferry_core::exec::{ExecError, Options, Request};
use serde_json::Value;

/// The session whose reasoning is replayed (upstream's
/// `xaiReasoningReplayScope`); none yet.
#[derive(Clone, Debug, Default)]
pub(crate) struct Scope;

/// Puts the session's kept reasoning back into `body`'s input
/// (`applyXAIReasoningReplayCacheRequired`). Does nothing yet.
pub(crate) fn apply(
    _body: &mut Value,
    _request: &Request,
    _options: &Options,
) -> Result<Scope, ExecError> {
    Ok(Scope)
}

/// Keeps the reasoning of a `response.completed` event's response
/// (`cacheXAIReasoningReplayFromCompleted`). Does nothing yet.
pub(crate) fn cache_completed(_scope: &Scope, _completed: &[u8]) {}

/// Forgets the session's kept reasoning once its history is compacted
/// (`clearXAIReasoningReplayAfterCompaction`). Does nothing yet.
pub(crate) fn clear_after_compaction(_scope: &Scope) {}
