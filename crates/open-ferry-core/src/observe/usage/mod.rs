//! Usage statistics: a record of each call's tokens, latency and outcome,
//! queued for the management API's usage queue, and the error events of
//! failed calls (upstream's sdk/cliproxy/usage, the usage reporter in
//! internal/runtime/executor/helps/usage_helpers.go and
//! internal/redisqueue). Not ported yet (P3 WP-C).
//!
//! What is here are the hooks the rest of the proxy calls, with the
//! signatures the port keeps: the binary makes the [`Usage`] at start,
//! [`reconfigure`]s it on every config load and gives the manager its
//! [`Usage::error_events`], and the server asks it for a [`Tap`] for each
//! call. For now it records nothing and gives no tap.
//!
//! A record's `session_id` may only come from a session header the client
//! sent, read from the call's [`Options::headers`]; it is never derived
//! (policy).
//!
//! Deviations from upstream: nothing is recorded yet.

mod error_events;

use std::sync::Arc;

use super::{RequestContext, Tap};
use crate::config::Config;
use crate::exec::{Options, Request};
use crate::manager::ErrorEvents;

#[cfg(test)]
mod tests;

/// The usage statistics. Cloning gives another handle to the same
/// statistics.
#[derive(Clone, Debug, Default)]
pub struct Usage {}

impl Usage {
    /// The statistics for `config`.
    pub fn new(config: &Config) -> Self {
        let _ = config;
        Self::default()
    }

    /// The tap that builds the usage record of a call made for the request
    /// of `context`, or `None` when none is kept.
    pub fn tap(
        &self,
        context: &Arc<RequestContext>,
        request: &Request,
        options: &Options,
    ) -> Option<Arc<dyn Tap>> {
        let _ = (context, request, options);
        None
    }

    /// What the manager tells about failed calls, for the usage queue's
    /// error subscribers (upstream's `publishErrorEvent`).
    pub fn error_events(&self) -> Arc<dyn ErrorEvents> {
        Arc::new(error_events::UsageErrorEvents::new(self))
    }
}

/// Applies `config` to `usage` (upstream's `redisqueue.SetEnabled`,
/// `SetUsageStatisticsEnabled` and `SetRetentionSeconds`, at start and on
/// reload). The queue is kept only while `management_available`: while the
/// management API serves requests (upstream's `managementRoutesEnabled`).
/// `previous` is the config before, `None` at start.
pub fn reconfigure(
    usage: &Usage,
    previous: Option<&Config>,
    config: &Config,
    management_available: bool,
) {
    let _ = (usage, previous, config, management_available);
}
