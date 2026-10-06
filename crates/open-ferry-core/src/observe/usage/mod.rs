// Ported from CLIProxyAPI internal/redisqueue/usage_toggle.go
// (SetUsageStatisticsEnabled, UsageStatisticsEnabled) and the usage
// queue's wiring in internal/api/server.go (managementRoutesEnabled) and
// sdk/cliproxy/service.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Usage statistics: a record of each executor call's tokens, latency and
//! outcome, queued for the management API's usage queue, and the error
//! events of failed calls (upstream's sdk/cliproxy/usage, the usage
//! reporter in internal/runtime/executor/helps/usage_helpers.go and
//! internal/redisqueue).
//!
//! The binary makes the [`Usage`] at start, [`reconfigure`]s it on every
//! config load and gives the manager its [`Usage::error_events`]; the
//! server asks it for a [`Tap`] for each call, which publishes a record
//! for each executor call (see the reporter's module). The management API
//! takes the records with [`Usage::pop_oldest`]; subscribers get them as
//! they come with [`Usage::subscribe_usage`] and the error events with
//! [`Usage::subscribe_errors`]. The usage ledger observes every record
//! with [`Usage::observe`], beside the queue and without taking any from
//! it (see the observer's module).
//!
//! The queue is kept only while the management API serves requests, and
//! records are made only while `usage-statistics-enabled` is on, for the
//! queue and the observer alike; both are read on every config load. A
//! call to a token count makes no record.
//!
//! The parsers of upstream answers ([`parse_openai_usage`] and the rest),
//! the token breakdown ([`ensure_token_breakdown_for_provider`]), the
//! TTFT classifiers ([`is_responses_token_event`] and the rest) and the
//! response model's checks ([`is_model_substituted`],
//! [`StreamResponseModelObserver`]) are public for the parity tool.
//!
//! A record's `session_id` may only come from a session header the client
//! sent, read from the call's [`Options::headers`]; it is never derived
//! (policy). Records keep the client's key and the credential's account in
//! clear, as upstream's do: the queue is only served to the management
//! API.
//!
//! Deviations from upstream:
//! - Records are made by a tap that reads the executor's traffic, not by
//!   the executors (see the reporter's module for what differs).
//! - The queue keeps at most [`MAX_QUEUED`] records (see the queue's
//!   module).
//! - The Redis protocol listener is not ported yet (P3 WP-F): until it is,
//!   the usage queue is served by the management API only.
//! - Records are also made, for the observer, while the queue is off: an
//!   observer is open-ferry's own.
//! - Not ported: session derivation and hierarchy, the
//!   Antigravity and Codex image tool parsers, the credits
//!   markers, and the usage plugins of the SDK's `usage.Manager` beyond
//!   the queue.

mod accounting;
mod error_events;
mod json;
mod observer;
mod parse;
mod queue;
mod record_json;
mod reporter;
mod response_model;
mod ttft;

use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use tokio::sync::mpsc;

pub use accounting::{
    Detail, InputBreakdown, OutputBreakdown, Quality, TOKEN_ACCOUNTING_SCHEMA_VERSION,
    TokenBreakdown, ensure_token_breakdown_for_provider,
};
pub use observer::{ClientKey, EventCredential, OBSERVER_BUFFER, Observation, UsageEvent};
pub use parse::{
    StreamUsageBuffer, merge_stream_usage_detail, parse_claude_stream_usage, parse_claude_usage,
    parse_codex_usage, parse_gemini_stream_usage, parse_gemini_usage,
    parse_interactions_stream_usage, parse_interactions_usage, parse_openai_stream_usage,
    parse_openai_usage,
};
pub use queue::{
    DEFAULT_RETENTION_SECONDS, MAX_QUEUED, MAX_RETENTION_SECONDS, SUBSCRIBER_BUFFER, Subscription,
    USAGE_REFRESH_PAYLOAD, USAGE_SUPPORT_REFRESH_PAYLOAD,
};
pub use response_model::{
    MAX_LINES_PER_STREAM_EVENT, STREAM_MODEL_BUFFER_BOUND, StreamResponseModelObserver,
    is_model_substituted,
};
pub use ttft::{
    is_chat_token_event, is_claude_token_event, is_gemini_token_event, is_responses_token_event,
};

use super::{RequestContext, Tap};
use crate::config::Config;
use crate::exec::{Options, Request};
use crate::manager::ErrorEvents;
use queue::Queue;
use response_model::{Clock, Throttle};

#[cfg(test)]
mod tests;

/// What the handles of one [`Usage`] share.
struct Inner {
    queue: Queue,
    throttle: Throttle,
    clock: Clock,
    observer: Arc<observer::Slot>,
}

/// The usage statistics. Cloning gives another handle to the same
/// statistics.
#[derive(Clone)]
pub struct Usage {
    inner: Arc<Inner>,
}

impl Default for Usage {
    fn default() -> Self {
        Self::with_clock(Arc::new(Instant::now))
    }
}

impl fmt::Debug for Usage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Usage")
            .field("enabled", &self.inner.queue.enabled())
            .field(
                "usage_statistics_enabled",
                &self.inner.queue.usage_statistics_enabled(),
            )
            .finish_non_exhaustive()
    }
}

impl Usage {
    /// The statistics for `config`, with the queue off until
    /// [`reconfigure`] turns it on.
    pub fn new(config: &Config) -> Self {
        let usage = Self::default();
        usage
            .inner
            .queue
            .set_usage_statistics_enabled(config.usage_statistics_enabled);
        usage
            .inner
            .queue
            .set_retention_seconds(config.redis_usage_queue_retention_seconds);
        usage
    }

    /// Statistics reading `clock` for the queue's retention, latencies and
    /// the warnings' throttle.
    pub(crate) fn with_clock(clock: Clock) -> Self {
        Self {
            inner: Arc::new(Inner {
                queue: Queue::new(Arc::clone(&clock)),
                throttle: Throttle::new(Arc::clone(&clock)),
                clock,
                observer: Arc::default(),
            }),
        }
    }

    /// The tap that builds the usage records of a call made with `request`
    /// and `options` for the request of `context`, or `None` when none is
    /// kept: records are off, neither the queue nor an observer would take
    /// them, or the call counts tokens.
    pub fn tap(
        &self,
        context: &Arc<RequestContext>,
        request: &Request,
        options: &Options,
    ) -> Option<Arc<dyn Tap>> {
        let queue = &self.inner.queue;
        if !queue.usage_statistics_enabled() || (!queue.enabled() && !self.inner.observer.present())
        {
            return None;
        }
        let path = &options.metadata.request_path;
        if path.ends_with("/count_tokens") || path.contains(":countTokens") {
            return None;
        }
        Some(Arc::new(reporter::UsageTap::new(
            Arc::clone(&self.inner),
            Arc::clone(context),
            request,
            options,
        )))
    }

    /// What the manager tells about failed calls, for the usage queue's
    /// error subscribers (upstream's `publishErrorEvent`).
    pub fn error_events(&self) -> Arc<dyn ErrorEvents> {
        Arc::new(error_events::UsageErrorEvents::new(self))
    }

    /// Whether the queue is on: while the management API serves requests
    /// (upstream's `Enabled`).
    pub fn enabled(&self) -> bool {
        self.inner.queue.enabled()
    }

    /// Sends `record` to the usage subscribers, or queues it when there are
    /// none; while the queue is off it is dropped (upstream's `Enqueue`).
    /// The taps publish each call's record this way.
    pub fn enqueue(&self, record: Bytes) {
        self.inner.queue.enqueue(record);
    }

    /// Takes up to `count` of the queued records, oldest first, as JSON
    /// (upstream's `PopOldest`).
    pub fn pop_oldest(&self, count: usize) -> Vec<Bytes> {
        self.inner.queue.pop_oldest(count)
    }

    /// Subscribes to the usage records as they are made; the first payload
    /// is [`USAGE_SUPPORT_REFRESH_PAYLOAD`] (upstream's `SubscribeUsage`).
    /// A subscriber that falls [`SUBSCRIBER_BUFFER`] payloads behind is
    /// dropped.
    pub fn subscribe_usage(&self) -> (mpsc::Receiver<Bytes>, Subscription) {
        self.inner.queue.subscribe_usage()
    }

    /// Observes every usage record as it is made, without taking it from the
    /// queue, until the [`Observation`] is dropped; a later observation ends
    /// this one. The receiver holds up to [`OBSERVER_BUFFER`] events; while
    /// it is full, events are lost and counted ([`Observation::dropped`]).
    pub fn observe(&self) -> (std::sync::mpsc::Receiver<UsageEvent>, Observation) {
        self.inner.observer.observe()
    }

    /// Whether records are made: `usage-statistics-enabled` is on.
    pub fn usage_statistics_enabled(&self) -> bool {
        self.inner.queue.usage_statistics_enabled()
    }

    /// Subscribes to the error events of failed calls (upstream's
    /// `SubscribeErrors`).
    pub fn subscribe_errors(&self) -> (mpsc::Receiver<Bytes>, Subscription) {
        self.inner.queue.subscribe_errors()
    }

    /// Tells the usage subscribers to refresh, with
    /// [`USAGE_REFRESH_PAYLOAD`] (upstream's `NotifyUsageRefresh`).
    pub fn notify_usage_refresh(&self) {
        self.inner.queue.notify_usage_refresh();
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
    let _ = previous;
    let queue = &usage.inner.queue;
    queue.set_enabled(management_available);
    queue.set_usage_statistics_enabled(config.usage_statistics_enabled);
    queue.set_retention_seconds(config.redis_usage_queue_retention_seconds);
}
