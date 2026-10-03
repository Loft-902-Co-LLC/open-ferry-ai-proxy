// The hook points of CLIProxyAPI's Codex executor, in
// internal/runtime/executor/codex_executor_execute.go and
// codex_executor_stream.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Hooks for the request rewrites whose effects reach past the request.
//!
//! Upstream's Codex executor rewrites a prepared body for multi-agent v2
//! and from the reasoning replay cache, undoes the first in each of Codex's
//! events, and as the call ends saves the turn's reasoning to the cache or
//! clears it. Here [`prepare`] makes such rewrites and notes in a [`Turn`]
//! what the other hooks need; [`restore`] rewrites each event, or a compact
//! call's answer, before anything reads it; and [`on_completed`] and
//! [`on_failure`] see how the call ended.
//!
//! None of those rewrites is ported yet, so each hook leaves things as they
//! are.

use std::borrow::Cow;

use open_ferry_core::exec::{Options, Request};
use serde_json::Value;

use super::request::{Context, Kind};

/// What [`prepare`] noted about a request, for the hooks that see its
/// response.
#[derive(Debug, Default)]
pub(crate) struct Turn {}

/// Rewrites the body of a call of `kind` other than a token count once it
/// is otherwise prepared, as upstream does after normalizing its tool
/// schemas, before the response translators' copy of it is taken.
pub(crate) fn prepare(
    _kind: Kind,
    _context: Context<'_>,
    _request: &Request,
    _options: &Options,
    _body: &mut Value,
) -> Turn {
    Turn::default()
}

/// The data of one of Codex's events, or a compact call's answer, as the
/// rest of the executor should read it.
pub(crate) fn restore<'d>(_turn: &Turn, data: &'d [u8]) -> Cow<'d, [u8]> {
    Cow::Borrowed(data)
}

/// Sees the terminal event of a call that succeeded (`response.completed`,
/// `response.incomplete` or `response.done`) as the executor passes it on.
pub(crate) fn on_completed(_turn: &Turn, _event: &Value) {}

/// Sees a call that failed with `status` and `body`: Codex's error status
/// and body, or a terminal failure event's status and error.
pub(crate) fn on_failure(_turn: &Turn, _status: u16, _body: &[u8]) {}
