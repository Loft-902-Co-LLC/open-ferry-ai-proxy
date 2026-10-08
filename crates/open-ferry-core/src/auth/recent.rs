// Ported from CLIProxyAPI sdk/cliproxy/auth/types.go (recentRequestRing,
// recordRecentRequest and RecentRequestsSnapshot) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A credential's recent calls: how many succeeded and how many failed in
//! each of the last twenty 10-minute windows, as the management API lists
//! them.
//!
//! Deviations from upstream: none.

use chrono::{Local, TimeZone};

use super::{Auth, Timestamp};

/// How long one window lasts, in seconds.
const BUCKET_SECONDS: i64 = 10 * 60;
/// How many windows are kept.
const BUCKET_COUNT: usize = 20;

/// The counts for one window, and which window they are for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Bucket {
    id: i64,
    success: i64,
    failed: i64,
}

/// The counts of a credential's calls over the last twenty windows
/// (upstream's `recentRequestRing`). Read them with
/// [`Auth::recent_requests_snapshot`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecentRequests {
    buckets: [Bucket; BUCKET_COUNT],
}

/// One window of [`Auth::recent_requests_snapshot`] (upstream's
/// `RecentRequestBucket`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentRequestBucket {
    /// The window in local time, as `15:00-15:10`.
    pub time: String,
    /// Calls that succeeded.
    pub success: i64,
    /// Calls that failed.
    pub failed: i64,
}

/// The window `now` falls in.
fn bucket_id(now: Timestamp) -> i64 {
    now.timestamp() / BUCKET_SECONDS
}

/// Where window `id` lives in the ring.
fn bucket_index(id: i64) -> usize {
    id.rem_euclid(BUCKET_COUNT as i64) as usize
}

/// Window `id` in local time, as `15:00-15:10`.
fn bucket_label(id: i64) -> String {
    let start_seconds = id.saturating_mul(BUCKET_SECONDS);
    let format = |seconds: i64| {
        Local
            .timestamp_opt(seconds, 0)
            .single()
            .map(|time| time.format("%H:%M").to_string())
            .unwrap_or_default()
    };
    format!(
        "{}-{}",
        format(start_seconds),
        format(start_seconds.saturating_add(BUCKET_SECONDS))
    )
}

impl Auth {
    /// Counts one call at `now` in its window (upstream's
    /// `recordRecentRequest`).
    pub(crate) fn record_recent_request(&mut self, now: Timestamp, success: bool) {
        let id = bucket_id(now);
        let bucket = &mut self.recent_requests.buckets[bucket_index(id)];
        if bucket.id != id {
            *bucket = Bucket {
                id,
                ..Bucket::default()
            };
        }
        if success {
            bucket.success = bucket.success.saturating_add(1);
        } else {
            bucket.failed = bucket.failed.saturating_add(1);
        }
    }

    /// The twenty windows up to the one `now` falls in, oldest first, with
    /// their counts (upstream's `RecentRequestsSnapshot`).
    pub fn recent_requests_snapshot(&self, now: Timestamp) -> Vec<RecentRequestBucket> {
        let current = bucket_id(now);
        (0..BUCKET_COUNT as i64)
            .rev()
            .map(|back| {
                let id = current - back;
                let bucket = self.recent_requests.buckets[bucket_index(id)];
                let (success, failed) = if bucket.id == id {
                    (bucket.success, bucket.failed)
                } else {
                    (0, 0)
                };
                RecentRequestBucket {
                    time: bucket_label(id),
                    success,
                    failed,
                }
            })
            .collect()
    }
}

/// Ported from upstream's `types_test.go` (the three `RecentRequestsSnapshot`
/// tests).
#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, Utc};

    use super::*;

    fn now() -> Timestamp {
        Utc.timestamp_opt(1_700_000_000, 0).unwrap()
    }

    #[test]
    fn snapshot_of_nothing_has_twenty_empty_windows() {
        let now = now();
        let got = Auth::default().recent_requests_snapshot(now);
        assert_eq!(got.len(), BUCKET_COUNT);
        let base = now.timestamp() / BUCKET_SECONDS - (BUCKET_COUNT as i64 - 1);
        for (i, bucket) in got.iter().enumerate() {
            assert_eq!((bucket.success, bucket.failed), (0, 0), "bucket {i}");
            let start = Local
                .timestamp_opt((base + i as i64) * BUCKET_SECONDS, 0)
                .unwrap();
            let end = start + TimeDelta::minutes(10);
            let expected = format!("{}-{}", start.format("%H:%M"), end.format("%H:%M"));
            assert_eq!(bucket.time, expected, "bucket {i}");
        }
    }

    #[test]
    fn snapshot_includes_counts() {
        let now = now();
        let mut auth = Auth::default();
        auth.record_recent_request(now, true);
        auth.record_recent_request(now, false);
        let got = auth.recent_requests_snapshot(now);
        assert_eq!(got.len(), BUCKET_COUNT);
        let newest = &got[BUCKET_COUNT - 1];
        assert_eq!((newest.success, newest.failed), (1, 1));
    }

    #[test]
    fn next_window_moves_counts_back() {
        let now = now();
        let next = now + TimeDelta::minutes(10);
        let mut auth = Auth::default();
        auth.record_recent_request(now, true);
        auth.record_recent_request(next, false);
        let got = auth.recent_requests_snapshot(next);
        assert_eq!(got.len(), BUCKET_COUNT);
        let second_newest = &got[BUCKET_COUNT - 2];
        assert_eq!((second_newest.success, second_newest.failed), (1, 0));
        let newest = &got[BUCKET_COUNT - 1];
        assert_eq!((newest.success, newest.failed), (0, 1));
    }

    #[test]
    fn a_window_left_behind_is_reused() {
        let now = now();
        let mut auth = Auth::default();
        auth.record_recent_request(now, true);
        // Twenty windows later the same slot holds the new window only.
        let later = now + TimeDelta::minutes(10 * BUCKET_COUNT as i64);
        auth.record_recent_request(later, false);
        let got = auth.recent_requests_snapshot(later);
        let totals = got
            .iter()
            .fold((0, 0), |(s, f), b| (s + b.success, f + b.failed));
        assert_eq!(totals, (0, 1));
        // And an old snapshot doesn't count a window from its future.
        let old = auth.recent_requests_snapshot(now);
        assert!(old.iter().all(|b| b.success == 0 && b.failed == 0));
    }
}
