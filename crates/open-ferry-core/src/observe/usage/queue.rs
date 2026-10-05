// Ported from CLIProxyAPI internal/redisqueue/queue.go (SetEnabled, Enabled,
// SetRetentionSeconds, Enqueue, EnqueueError, PopOldest, SubscribeUsage,
// SubscribeErrors, NotifyUsageRefresh, publishToSubscribers, subscribe,
// pruneLocked) and usage_toggle.go (SetUsageStatisticsEnabled,
// UsageStatisticsEnabled) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The usage queue: the usage records waiting for the management API to
//! take them, and the subscribers that get records and error events as
//! they come.
//!
//! The queue is kept only while it is enabled, which the proxy does while
//! the management API is available. A record goes to the usage subscribers
//! when there are any, and is queued otherwise; queued records are kept for
//! the retention time (60 seconds unless configured, at most an hour), and
//! [`Queue::pop_oldest`] takes them oldest first. An error event goes only
//! to the error subscribers. A subscriber is sent its payloads through a
//! channel of 256; one that falls that far behind is dropped, its channel
//! closed, so that publishing never waits.
//!
//! Deviations from upstream:
//! - The queue keeps at most [`MAX_QUEUED`] records; past that the oldest
//!   are dropped, with a warning at most once a minute. Upstream's grows
//!   without bound within its retention.
//! - The queue belongs to the [`super::Usage`], where upstream keeps one for
//!   the process.
//! - Upstream's RESP server, the subscribers' only user, isn't ported yet.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::mpsc;

use super::response_model::Clock;

/// How long records are kept unless configured (upstream's
/// `defaultRetentionSeconds`).
pub const DEFAULT_RETENTION_SECONDS: i64 = 60;

/// The longest records may be kept (upstream's `maxRetentionSeconds`).
pub const MAX_RETENTION_SECONDS: i64 = 3600;

/// How many payloads a subscriber may fall behind before it is dropped
/// (upstream's `usageSubscriberBuffer` and `errorSubscriberBuffer`).
pub const SUBSCRIBER_BUFFER: usize = 256;

/// The most records the queue keeps.
pub const MAX_QUEUED: usize = 100_000;

/// What a new usage subscriber is sent first: that the queue can tell it to
/// refresh (upstream's `usageSupportRefreshPayload`).
pub const USAGE_SUPPORT_REFRESH_PAYLOAD: &str = r#"{"support_refresh":true}"#;

/// What usage subscribers are sent to refresh (upstream's
/// `usageRefreshPayload`).
pub const USAGE_REFRESH_PAYLOAD: &str = r#"{"refresh":true}"#;

/// How often dropping records past [`MAX_QUEUED`] warns.
const DROP_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// Which of the queue's two lanes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaneKind {
    Usage,
    Errors,
}

/// A queued record.
#[derive(Debug)]
struct Item {
    enqueued_at: Instant,
    payload: Bytes,
}

/// The records of one lane, and its subscribers.
#[derive(Debug, Default)]
struct Lane {
    items: VecDeque<Item>,
    subscribers: BTreeMap<u64, mpsc::Sender<Bytes>>,
    next_subscriber: u64,
    dropped: u64,
    last_drop_warn: Option<Instant>,
}

impl Lane {
    /// Forgets everything; dropping the senders closes the subscribers'
    /// channels.
    fn clear(&mut self) {
        self.items = VecDeque::new();
        self.subscribers = BTreeMap::new();
    }

    /// Sends `payload` to every subscriber, dropping those that are full or
    /// gone; whether there were any.
    fn publish(&mut self, payload: &Bytes) -> bool {
        if self.subscribers.is_empty() {
            return false;
        }
        self.subscribers
            .retain(|_, sender| sender.try_send(payload.clone()).is_ok());
        true
    }
}

pub(crate) struct State {
    enabled: AtomicBool,
    usage_statistics_enabled: AtomicBool,
    retention_seconds: AtomicI64,
    usage: Mutex<Lane>,
    errors: Mutex<Lane>,
    clock: Clock,
}

/// The usage queue and its subscribers. Cloning gives another handle to the
/// same queue.
#[derive(Clone)]
pub(crate) struct Queue(Arc<State>);

impl Queue {
    /// A disabled queue reading `clock`.
    pub(crate) fn new(clock: Clock) -> Self {
        Self(Arc::new(State {
            enabled: AtomicBool::new(false),
            usage_statistics_enabled: AtomicBool::new(true),
            retention_seconds: AtomicI64::new(DEFAULT_RETENTION_SECONDS),
            usage: Mutex::new(Lane::default()),
            errors: Mutex::new(Lane::default()),
            clock,
        }))
    }

    fn lane(&self, kind: LaneKind) -> MutexGuard<'_, Lane> {
        lane_of(&self.0, kind)
    }

    /// Turns the queue on or off; off, it forgets its records and drops its
    /// subscribers (upstream's `SetEnabled`).
    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.0.enabled.store(enabled, Ordering::SeqCst);
        if !enabled {
            self.lane(LaneKind::Usage).clear();
            self.lane(LaneKind::Errors).clear();
        }
    }

    /// Whether the queue is on (upstream's `Enabled`).
    pub(crate) fn enabled(&self) -> bool {
        self.0.enabled.load(Ordering::SeqCst)
    }

    /// Turns the recording of usage on or off (upstream's
    /// `SetUsageStatisticsEnabled`).
    pub(crate) fn set_usage_statistics_enabled(&self, enabled: bool) {
        self.0
            .usage_statistics_enabled
            .store(enabled, Ordering::SeqCst);
    }

    /// Whether usage is recorded (upstream's `UsageStatisticsEnabled`).
    pub(crate) fn usage_statistics_enabled(&self) -> bool {
        self.0.usage_statistics_enabled.load(Ordering::SeqCst)
    }

    /// Sets how long records are kept: 60 seconds for zero or less, at most
    /// an hour (upstream's `SetRetentionSeconds`).
    pub(crate) fn set_retention_seconds(&self, seconds: i64) {
        let seconds = if seconds <= 0 {
            DEFAULT_RETENTION_SECONDS
        } else {
            seconds.min(MAX_RETENTION_SECONDS)
        };
        self.0.retention_seconds.store(seconds, Ordering::SeqCst);
    }

    fn retention(&self) -> Duration {
        let seconds = self.0.retention_seconds.load(Ordering::SeqCst);
        Duration::from_secs(u64::try_from(seconds).unwrap_or(0))
    }

    /// Sends a usage record to the subscribers, or queues it when there are
    /// none (upstream's `Enqueue`).
    pub(crate) fn enqueue(&self, payload: Bytes) {
        if !self.enabled() || payload.is_empty() {
            return;
        }
        let now = (self.0.clock)();
        let retention = self.retention();
        let mut lane = self.lane(LaneKind::Usage);
        if lane.publish(&payload) {
            return;
        }
        prune(&mut lane, now, retention);
        if lane.items.len() >= MAX_QUEUED {
            let excess = lane.items.len() + 1 - MAX_QUEUED;
            lane.items.drain(..excess);
            lane.dropped = lane.dropped.wrapping_add(excess as u64);
            let due = lane
                .last_drop_warn
                .is_none_or(|at| now.saturating_duration_since(at) >= DROP_WARN_INTERVAL);
            if due {
                tracing::warn!(
                    dropped = lane.dropped,
                    "usage queue full at {MAX_QUEUED} records: dropping the oldest"
                );
                lane.dropped = 0;
                lane.last_drop_warn = Some(now);
            }
        }
        lane.items.push_back(Item {
            enqueued_at: now,
            payload,
        });
    }

    /// Sends an error event to the error subscribers; without any it is
    /// dropped (upstream's `EnqueueError`).
    pub(crate) fn enqueue_error(&self, payload: Bytes) {
        if !self.enabled() || payload.is_empty() {
            return;
        }
        self.lane(LaneKind::Errors).publish(&payload);
    }

    /// Takes up to `count` of the queued records, oldest first, past those
    /// older than the retention (upstream's `PopOldest`).
    pub(crate) fn pop_oldest(&self, count: usize) -> Vec<Bytes> {
        if !self.enabled() || count == 0 {
            return Vec::new();
        }
        let now = (self.0.clock)();
        let retention = self.retention();
        let mut lane = self.lane(LaneKind::Usage);
        prune(&mut lane, now, retention);
        let take = count.min(lane.items.len());
        lane.items.drain(..take).map(|item| item.payload).collect()
    }

    /// Subscribes to usage records; the first payload says the queue can
    /// tell the subscriber to refresh (upstream's `SubscribeUsage`).
    pub(crate) fn subscribe_usage(&self) -> (mpsc::Receiver<Bytes>, Subscription) {
        self.subscribe(
            LaneKind::Usage,
            Some(Bytes::from_static(USAGE_SUPPORT_REFRESH_PAYLOAD.as_bytes())),
        )
    }

    /// Subscribes to error events (upstream's `SubscribeErrors`).
    pub(crate) fn subscribe_errors(&self) -> (mpsc::Receiver<Bytes>, Subscription) {
        self.subscribe(LaneKind::Errors, None)
    }

    /// Tells the usage subscribers to refresh (upstream's
    /// `NotifyUsageRefresh`).
    pub(crate) fn notify_usage_refresh(&self) {
        self.lane(LaneKind::Usage)
            .publish(&Bytes::from_static(USAGE_REFRESH_PAYLOAD.as_bytes()));
    }

    fn subscribe(
        &self,
        kind: LaneKind,
        initial: Option<Bytes>,
    ) -> (mpsc::Receiver<Bytes>, Subscription) {
        let (sender, receiver) = mpsc::channel(SUBSCRIBER_BUFFER);
        if let Some(initial) = initial {
            let _ = sender.try_send(initial);
        }
        let mut lane = self.lane(kind);
        let id = lane.next_subscriber;
        lane.next_subscriber = lane.next_subscriber.wrapping_add(1);
        lane.subscribers.insert(id, sender);
        let subscription = Subscription {
            state: Arc::downgrade(&self.0),
            kind,
            id,
        };
        (receiver, subscription)
    }

    /// How many error events are queued; always none.
    #[cfg(test)]
    pub(crate) fn queued_errors(&self) -> usize {
        self.lane(LaneKind::Errors).items.len()
    }
}

fn lane_of(state: &State, kind: LaneKind) -> MutexGuard<'_, Lane> {
    let lane = match kind {
        LaneKind::Usage => &state.usage,
        LaneKind::Errors => &state.errors,
    };
    lane.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Drops the records queued before the retention (upstream's
/// `pruneLocked`).
fn prune(lane: &mut Lane, now: Instant, retention: Duration) {
    let Some(cutoff) = now.checked_sub(retention) else {
        return;
    };
    while lane
        .items
        .front()
        .is_some_and(|item| item.enqueued_at < cutoff)
    {
        lane.items.pop_front();
    }
}

/// A subscription to the usage queue. Unsubscribing, or dropping it, closes
/// its channel (upstream's unsubscribe function).
#[derive(Debug)]
pub struct Subscription {
    state: Weak<State>,
    kind: LaneKind,
    id: u64,
}

impl Subscription {
    /// Ends the subscription; once ended, ending it again does nothing.
    pub fn unsubscribe(&self) {
        if let Some(state) = self.state.upgrade() {
            lane_of(&state, self.kind).subscribers.remove(&self.id);
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Queue")
            .field("enabled", &self.enabled.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Queue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
