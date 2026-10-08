// Ported from CLIProxyAPI internal/cache/antigravity_reasoning_replay_cache_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests for the in-process replay cache: a later write replacing an earlier
//! one, eviction, misses leaving nothing behind, and items cut down as they
//! are stored. Each test uses a `ReplayCache` of its own with an explicit
//! clock, where upstream clears the process-wide cache.
//!
//! Dropped or changed tests:
//! - antigravity_reasoning_replay_conditional_mutation_rejects_stale_local_snapshot:
//!   changed; snapshots and conditional replace and delete aren't ported, so
//!   only the plain writes and reads remain: the newer write is what a read
//!   finds.
//! - antigravity_reasoning_replay_non_prefix_replace_rotates_local_branch:
//!   dropped; snapshots, conditional replace and branches aren't ported.
//! - antigravity_reasoning_replay_conditional_replace_accepts_descendant_local_chain:
//!   dropped; snapshots, conditional replace and branches aren't ported.
//! - antigravity_reasoning_replay_descendant_merge_rejects_reset_branch_aba:
//!   dropped; snapshots, conditional replace and delete, and branches aren't
//!   ported.
//! - antigravity_reasoning_replay_conditional_delete_tombstone_blocks_stale_first_writer:
//!   dropped; snapshots, revisions and conditional replace and delete aren't
//!   ported.
//! - antigravity_reasoning_replay_evicted_tombstone_still_blocks_stale_first_writer:
//!   dropped; snapshots, revisions and conditional replace and delete aren't
//!   ported.
//! - antigravity_reasoning_replay_unrelated_eviction_does_not_block_absent_snapshot:
//!   changed; with no snapshot or conditional write and no marker for a miss,
//!   it checks that the miss leaves only the live entry, that evicting the
//!   oldest entry takes it, and that a first write for the missed session is
//!   then kept. The live entry is made older by moving the clock on, where
//!   upstream backdates its timestamp.
//! - antigravity_reasoning_replay_home_absent_snapshot_is_fenced: dropped; the
//!   Home store isn't ported.
//! - antigravity_reasoning_replay_conditional_mutation_rejects_stale_home_snapshot:
//!   dropped; the Home store isn't ported.
//! - antigravity_reasoning_replay_non_prefix_replace_rotates_home_branch:
//!   dropped; the Home store isn't ported.
//! - antigravity_reasoning_replay_conditional_replace_accepts_descendant_home_chain:
//!   dropped; the Home store isn't ported.
//! - antigravity_reasoning_replay_home_generation_rejects_successful_value_aba:
//!   dropped; the Home store isn't ported.
//! - antigravity_reasoning_replay_home_reports_cas_errors: dropped; the Home
//!   store isn't ported.
//! - antigravity_reasoning_replay_home_cas_retry_rejects_oversized_value:
//!   dropped; the Home store isn't ported.
//! - antigravity_reasoning_replay_local_tombstones_stay_within_entry_bound:
//!   dropped; deleting an entry isn't ported, as the translator never deletes
//!   one.
//! - antigravity_reasoning_replay_local_absence_reservations_stay_within_entry_bound:
//!   changed; a miss leaves no marker here, so it checks that more misses than
//!   the entry limit leave the cache empty.
//! - antigravity_reasoning_replay_home_writes_remain_legacy_array_readable:
//!   dropped; the Home store isn't ported.
//! - antigravity_reasoning_replay_home_read_normalizes_and_rejects_mixed_invalid_chain:
//!   changed; only the part that holds for the in-process store is kept: a
//!   `function_call_part` item keeps its `targetOccurrence` when stored and
//!   read back. The Home read's TTL refresh and its rejection of a stored
//!   chain holding an invalid item are Home-only.

use serde_json::{Value, json};

use super::*;

const MODEL: &str = "gemini-3.6-flash-high";

/// `antigravityReplayTestItem`.
fn replay_test_item(signature: &str) -> Value {
    json!({
        "type": "thought_signature",
        "contentIndex": 1,
        "partIndex": 0,
        "thoughtSignature": signature,
    })
}

#[test]
fn antigravity_reasoning_replay_conditional_mutation_rejects_stale_local_snapshot() {
    const SESSION: &str = "stale-local";
    let mut cache = ReplayCache::default();
    let now = Instant::now();
    let old_item = replay_test_item("old-local-signature-123456");
    let new_item = replay_test_item("new-local-signature-123456");
    assert!(
        cache.cache(MODEL, SESSION, &[old_item], now),
        "initial cache write failed"
    );
    assert!(
        cache.get(MODEL, SESSION, now).is_some(),
        "read failed: found=false"
    );
    assert!(
        cache.cache(MODEL, SESSION, &[new_item], now),
        "newer cache write failed"
    );
    let items = cache.get(MODEL, SESSION, now);
    assert!(
        items.as_ref().is_some_and(
            |items| items.len() == 1 && items[0].to_string().contains("new-local-signature")
        ),
        "newer state was lost: {items:?}"
    );
}

#[test]
fn antigravity_reasoning_replay_unrelated_eviction_does_not_block_absent_snapshot() {
    const ABSENT_SESSION: &str = "untouched-absent-session";
    let mut cache = ReplayCache::default();
    let start = Instant::now();
    let live_item = replay_test_item("evicted-live-signature-123456");
    assert!(
        cache.cache(MODEL, "older-live-entry", &[live_item], start),
        "live entry write failed"
    );
    // Upstream backdates the live entry by a minute; here the clock moves on.
    let now = start + Duration::from_secs(60);
    let found = cache.get(MODEL, ABSENT_SESSION, now);
    assert!(found.is_none(), "initial absent read = {found:?}");
    assert_eq!(cache.len(), 1, "the miss left an entry behind");
    cache.evict_oldest(1);
    assert_eq!(cache.len(), 0, "eviction kept the live entry");
    let first_item = replay_test_item("first-write-after-unrelated-eviction-123456");
    assert!(
        cache.cache(MODEL, ABSENT_SESSION, &[first_item], now),
        "unrelated eviction blocked first write"
    );
    let items = cache.get(MODEL, ABSENT_SESSION, now);
    assert!(
        items.as_ref().is_some_and(|items| items.len() == 1
            && items[0]
                .to_string()
                .contains("first-write-after-unrelated-eviction")),
        "first write was lost: {items:?}"
    );
}

#[test]
fn antigravity_reasoning_replay_local_absence_reservations_stay_within_entry_bound() {
    // Misses leave nothing behind here, so they can't fill the cache at all.
    let mut cache = ReplayCache::default();
    let start = Instant::now();
    for index in 0..=MAX_ENTRIES {
        let session = format!("absent-reservation-{index}");
        let now = start + Duration::from_millis(index as u64);
        let found = cache.get(MODEL, &session, now);
        assert!(
            found.is_none(),
            "absence reservation {index} = found {found:?}"
        );
    }
    assert_eq!(cache.len(), 0, "misses left entries behind");
}

#[test]
fn long_session_ids_are_hashed() {
    let mut cache = ReplayCache::default();
    let now = Instant::now();
    let session = "x".repeat(1 << 20);
    assert!(cache.get(MODEL, &session, now).is_none());
    assert_eq!(cache.len(), 0, "a miss left an entry behind");
    let item = replay_test_item("long-session-signature-123456");
    assert!(cache.cache(MODEL, &session, &[item], now));
    assert!(cache.get(MODEL, &session, now).is_some());
    assert!(
        cache.get(MODEL, &session[1..], now).is_none(),
        "another session shares the entry"
    );
    assert_eq!(cache.len(), 1);
}

#[test]
fn antigravity_reasoning_replay_home_read_normalizes_and_rejects_mixed_invalid_chain() {
    const SESSION: &str = "home-validation";
    let valid: Value = serde_json::from_str(
        r#"{"type":"function_call_part","name":"run","args":{"b":2,"a":1},"targetOccurrence":1,"thoughtSignature":"valid-home-signature-123456"}"#,
    )
    .expect("valid test JSON");
    let mut cache = ReplayCache::default();
    let now = Instant::now();
    assert!(
        cache.cache(MODEL, SESSION, &[valid], now),
        "valid write failed"
    );
    let items = cache.get(MODEL, SESSION, now);
    let Some([item]) = items.as_deref() else {
        panic!("valid read = {items:?}");
    };
    let item = item.to_string();
    assert!(
        item.contains(r#""targetOccurrence":1"#),
        "target occurrence was not normalized: {item}"
    );
}
