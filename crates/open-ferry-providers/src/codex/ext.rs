// The hook points of CLIProxyAPI's Codex executor, in
// internal/runtime/executor/codex_executor_execute.go and
// codex_executor_stream.go (v8.0.15, MIT).
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
//! Multi-agent v2 and orphan delegation are ported (see the `compat`
//! module): [`prepare`] readies a Codex client's request and [`restore`]
//! names the collaboration namespace back. The reasoning replay cache is
//! ported, in [`super::replay`]: [`prepare`] then replays a Claude client's
//! earlier turns, [`on_completed`] saves its turn and [`on_failure`] clears
//! them.

use std::borrow::Cow;

use open_ferry_core::exec::{Options, Request};
use open_ferry_translate::codex_client::multi_agent_v2;
use serde_json::Value;

use super::compat;
use super::replay;
use super::request::{Context, Kind};

/// What [`prepare`] noted about a request, for the hooks that see its
/// response.
#[derive(Clone, Debug, Default)]
pub(crate) struct Turn {
    /// Whether the collaboration namespace was renamed for multi-agent v2,
    /// so that responses must name it back.
    multi_agent_v2_optimized: bool,
    /// Where the reasoning replay reads and saves the turn.
    replay: replay::Scope,
}

impl Turn {
    /// Whether [`prepare`] renamed the collaboration namespace for
    /// multi-agent v2.
    pub(crate) fn multi_agent_v2_optimized(&self) -> bool {
        self.multi_agent_v2_optimized
    }

    /// Sets whether [`restore`] names the collaboration namespace back. The
    /// WebSocket upstream does so on a connection where an earlier request
    /// renamed it, too.
    pub(crate) fn set_multi_agent_v2_restore(&mut self, restore: bool) {
        self.multi_agent_v2_optimized = restore;
    }
}

/// Rewrites the body of a call of `kind` other than a token count once it
/// is otherwise prepared, as upstream does after normalizing its tool
/// schemas, before the response translators' copy of it is taken.
pub(crate) fn prepare(
    kind: Kind,
    context: Context<'_>,
    request: &Request,
    options: &Options,
    body: &mut Value,
) -> Turn {
    let multi_agent_v2_optimized = compat::prepare(context, request, options, body);
    let replay = replay::prepare(kind, request, options, body);
    Turn {
        multi_agent_v2_optimized,
        replay,
    }
}

/// The data of one of Codex's events, or a compact call's answer, as the
/// rest of the executor should read it.
pub(crate) fn restore<'d>(turn: &Turn, data: &'d [u8]) -> Cow<'d, [u8]> {
    multi_agent_v2::restore_response(data, turn.multi_agent_v2_optimized)
}

/// Sees the terminal event of a call that succeeded (`response.completed`,
/// `response.incomplete` or `response.done`) as the executor passes it on.
pub(crate) fn on_completed(turn: &Turn, event: &Value) {
    replay::on_completed(&turn.replay, event);
}

/// Sees a call that failed with `status` and `body`: Codex's error status
/// and body, or a terminal failure event's status and error.
pub(crate) fn on_failure(turn: &Turn, status: u16, body: &[u8]) {
    replay::on_failure(&turn.replay, status, body);
}
