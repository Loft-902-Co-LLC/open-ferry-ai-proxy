// Ported from CLIProxyAPI internal/redisqueue/queue_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of the usage queue and its subscribers. All of upstream's are
//! ported.
//!
//! Deviations from upstream: each test has its own queue, where upstream's
//! share the process's; a subscriber's channel is read without waiting,
//! since publishing sends before it returns.

use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::error::TryRecvError;

use super::super::queue::{
    MAX_QUEUED, Queue, SUBSCRIBER_BUFFER, USAGE_REFRESH_PAYLOAD, USAGE_SUPPORT_REFRESH_PAYLOAD,
};
use super::support::ManualClock;

fn enabled_queue() -> (Queue, ManualClock) {
    let clock = ManualClock::new();
    let queue = Queue::new(clock.clock());
    queue.set_enabled(true);
    (queue, clock)
}

#[track_caller]
fn require_payload(subscriber: &mut Receiver<Bytes>, want: &str) {
    match subscriber.try_recv() {
        Ok(got) => assert_eq!(got, want.as_bytes(), "subscriber payload"),
        Err(error) => panic!("no subscriber payload {want:?}: {error}"),
    }
}

fn popped(queue: &Queue, count: usize) -> Vec<String> {
    queue
        .pop_oldest(count)
        .iter()
        .map(|item| String::from_utf8_lossy(item).into_owned())
        .collect()
}

/// Ports TestEnqueueBroadcastsToUsageSubscribersAndSkipsQueue.
#[test]
fn enqueue_broadcasts_to_usage_subscribers_and_skips_queue() {
    let (queue, _clock) = enabled_queue();
    let (mut first, first_subscription) = queue.subscribe_usage();
    let (mut second, second_subscription) = queue.subscribe_usage();
    require_payload(&mut first, USAGE_SUPPORT_REFRESH_PAYLOAD);
    require_payload(&mut second, USAGE_SUPPORT_REFRESH_PAYLOAD);

    queue.enqueue(Bytes::from_static(b"usage-record"));
    require_payload(&mut first, "usage-record");
    require_payload(&mut second, "usage-record");
    assert!(popped(&queue, 1).is_empty(), "queued after a broadcast");

    first_subscription.unsubscribe();
    drop(second_subscription);
    queue.enqueue(Bytes::from_static(b"queued-record"));
    assert_eq!(popped(&queue, 1), ["queued-record"]);
}

/// Ports TestSetEnabledFalseClosesUsageSubscribers.
#[test]
fn set_enabled_false_closes_usage_subscribers() {
    let (queue, _clock) = enabled_queue();
    let (mut subscriber, _subscription) = queue.subscribe_usage();
    let (mut errors, _error_subscription) = queue.subscribe_errors();
    require_payload(&mut subscriber, USAGE_SUPPORT_REFRESH_PAYLOAD);

    queue.set_enabled(false);
    assert_eq!(subscriber.try_recv(), Err(TryRecvError::Disconnected));
    assert_eq!(errors.try_recv(), Err(TryRecvError::Disconnected));
}

/// Ports TestEnqueueErrorBroadcastsToErrorSubscribersAndDiscardsWithoutSubscribers.
#[test]
fn enqueue_error_broadcasts_to_error_subscribers_and_discards_without_subscribers() {
    let (queue, _clock) = enabled_queue();
    let (mut subscriber, subscription) = queue.subscribe_errors();
    queue.enqueue_error(Bytes::from_static(b"error-record"));
    require_payload(&mut subscriber, "error-record");

    subscription.unsubscribe();
    queue.enqueue_error(Bytes::from_static(b"discarded-error"));
    assert_eq!(queue.queued_errors(), 0);
    assert!(popped(&queue, 1).is_empty());
}

/// Ports TestNotifyUsageRefreshBroadcastsOnlyToUsageSubscribers.
#[test]
fn notify_usage_refresh_broadcasts_only_to_usage_subscribers() {
    let (queue, _clock) = enabled_queue();
    let (mut subscriber, subscription) = queue.subscribe_usage();
    let (mut errors, _error_subscription) = queue.subscribe_errors();
    require_payload(&mut subscriber, USAGE_SUPPORT_REFRESH_PAYLOAD);

    queue.notify_usage_refresh();
    require_payload(&mut subscriber, USAGE_REFRESH_PAYLOAD);
    assert_eq!(errors.try_recv(), Err(TryRecvError::Empty));

    subscription.unsubscribe();
    queue.notify_usage_refresh();
    assert!(popped(&queue, 1).is_empty());
}

/// Not upstream's: records older than the retention are dropped, which is
/// 60 seconds for zero and at most an hour.
#[test]
fn pop_oldest_drops_records_past_the_retention() {
    let (queue, clock) = enabled_queue();
    queue.set_retention_seconds(0);
    queue.enqueue(Bytes::from_static(b"old"));
    clock.advance(Duration::from_secs(30));
    queue.enqueue(Bytes::from_static(b"new"));
    clock.advance(Duration::from_secs(31));
    assert_eq!(popped(&queue, 10), ["new"]);

    queue.set_retention_seconds(i64::MAX);
    queue.enqueue(Bytes::from_static(b"kept"));
    clock.advance(Duration::from_secs(3600));
    assert_eq!(popped(&queue, 10), ["kept"]);
    queue.enqueue(Bytes::from_static(b"expired"));
    clock.advance(Duration::from_secs(3601));
    assert!(popped(&queue, 10).is_empty());
}

/// Not upstream's: records come oldest first, `count` at a time, and none
/// while the queue is off; turning it off forgets them.
#[test]
fn pop_oldest_takes_oldest_first_and_nothing_while_off() {
    let (queue, _clock) = enabled_queue();
    for record in ["a", "b", "c"] {
        queue.enqueue(Bytes::from(record));
    }
    queue.enqueue(Bytes::new());
    assert!(popped(&queue, 0).is_empty());
    assert_eq!(popped(&queue, 2), ["a", "b"]);
    queue.set_enabled(false);
    assert!(popped(&queue, 10).is_empty());
    queue.enqueue(Bytes::from_static(b"while-off"));
    queue.set_enabled(true);
    assert!(popped(&queue, 10).is_empty());
}

/// Not upstream's: the queue keeps at most [`MAX_QUEUED`] records,
/// dropping the oldest.
#[test]
fn enqueue_drops_the_oldest_past_the_cap() {
    let (queue, _clock) = enabled_queue();
    for index in 0..MAX_QUEUED + 2 {
        queue.enqueue(Bytes::from(index.to_string()));
    }
    let records = popped(&queue, MAX_QUEUED + 10);
    assert_eq!(records.len(), MAX_QUEUED);
    assert_eq!(records.first().map(String::as_str), Some("2"));
    let last = (MAX_QUEUED + 1).to_string();
    assert_eq!(records.last(), Some(&last));
}

/// Not upstream's: a subscriber that falls [`SUBSCRIBER_BUFFER`] payloads
/// behind is dropped, the record it couldn't take lost, as upstream's is;
/// later records are queued.
#[test]
fn a_full_subscriber_is_dropped() {
    let (queue, _clock) = enabled_queue();
    let (mut subscriber, _subscription) = queue.subscribe_usage();
    for index in 0..SUBSCRIBER_BUFFER {
        queue.enqueue(Bytes::from(index.to_string()));
    }
    queue.enqueue(Bytes::from_static(b"queued"));
    let mut received = 0;
    while subscriber.try_recv().is_ok() {
        received += 1;
    }
    assert_eq!(received, SUBSCRIBER_BUFFER);
    assert_eq!(subscriber.try_recv(), Err(TryRecvError::Disconnected));
    assert_eq!(popped(&queue, 10), ["queued"]);
}
