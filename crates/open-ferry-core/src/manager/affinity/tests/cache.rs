// Ported from CLIProxyAPI sdk/cliproxy/auth/session_cache_test.go, and the
// session cache tests of selector_test.go, selector_lcp_test.go and
// session_affinity_metadata_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of the session cache: binding, refreshing, unbinding, expiry, the
//! cap on keys and the cap on a session's aliases.
//!
//! Deviations from upstream:
//! - The time is passed in: `SessionCache_GetAndRefresh` moves it on where
//!   upstream sleeps.
//! - `SessionCache_ConcurrentSaturatedAccess` interleaves its eight
//!   workers' calls on one thread, since the cache has no lock of its own
//!   (the manager's guards it).
//! - Tests that read upstream's `entries` map read `contains` and
//!   `group_len`, as the keys are held hashed.
//! - Not ported: `SessionCache_NilReceiverSafety`,
//!   `SessionCache_StopNilChannelNoPanic` and `SessionCache_StopConcurrent`,
//!   as there is no nil cache and no cleanup goroutine to stop.

use std::time::Duration;

use super::{at, base};
use crate::manager::affinity::cache::SessionCache;

const HOUR: Duration = Duration::from_secs(60 * 60);

// TestSessionCache_CapacityEvictionOrder.
#[test]
fn capacity_eviction_order() {
    let now = base();
    let mut cache = SessionCache::with_capacity(HOUR, 5);
    for i in 1..=5 {
        cache.set(&format!("sess-{i}"), &format!("auth-{i}"), now);
    }
    assert_eq!(cache.len(), 5);

    // A sixth key evicts the oldest.
    cache.set("sess-6", "auth-6", now);
    assert!(cache.len() <= 5, "len {}", cache.len());
    assert_eq!(cache.get("sess-1", now), None, "sess-1 should be evicted");
    for i in 2..=6 {
        assert_eq!(
            cache.get(&format!("sess-{i}"), now),
            Some(format!("auth-{i}"))
        );
    }
}

// TestSessionCache_MultiAliasGroupEviction.
#[test]
fn multi_alias_group_eviction() {
    let now = base();
    let mut cache = SessionCache::with_capacity(HOUR, 4);
    cache.set_aliases("auth-1", &["s1-a", "s1-b"], now);
    cache.set_aliases("auth-2", &["s2-a", "s2-b"], now);
    assert_eq!(cache.len(), 4);

    // One more key evicts the whole oldest group.
    cache.set("s3", "auth-3", now);
    assert!(cache.len() <= 4, "len {}", cache.len());
    assert_eq!(cache.get("s1-a", now), None);
    assert_eq!(cache.get("s1-b", now), None);
    assert_eq!(cache.get("s2-a", now).as_deref(), Some("auth-2"));
    assert_eq!(cache.get("s3", now).as_deref(), Some("auth-3"));
}

// TestSessionCache_HighThroughputCapacitySaturated.
#[test]
fn high_throughput_capacity_saturated() {
    let now = base();
    let mut cache = SessionCache::with_capacity(HOUR, 1000);
    for i in 0..10_000 {
        cache.set(&format!("sess-{i}"), &format!("auth-{}", i % 10), now);
    }
    assert!(cache.len() <= 1000, "len {}", cache.len());
    for i in 9900..10_000 {
        assert_eq!(
            cache.get(&format!("sess-{i}"), now),
            Some(format!("auth-{}", i % 10)),
            "sess-{i}"
        );
    }
}

// TestSessionCache_ConcurrentSaturatedAccess, with the workers' calls
// interleaved on one thread.
#[test]
fn concurrent_saturated_access() {
    let now = base();
    let mut cache = SessionCache::with_capacity(HOUR, 50);
    for i in 0..500 {
        for worker in 0..8 {
            let key = format!("worker-{worker}-sess-{i}");
            let auth = format!("auth-{worker}");
            cache.set(&key, &auth, now);
            cache.get(&key, now);
            if i % 5 == 0 {
                cache.touch(&key, &auth, now);
            }
            if i % 7 == 0 {
                cache.compare_and_delete(&key, &auth, now);
            }
        }
    }
    assert!(cache.len() <= 50, "len {}", cache.len());
}

// TestSessionCacheCapacityBounding (session_affinity_metadata_test.go).
#[test]
fn capacity_bounding() {
    let now = base();
    let mut cache = SessionCache::with_capacity(HOUR, 10);
    for i in 0..20 {
        cache.set(&format!("session-{i}"), &format!("auth-{i}"), now);
    }
    assert!(cache.len() <= 10, "len {}", cache.len());
}

// TestSessionCacheSharedPromptKeyCapsStableAliasesByRecency
// (selector_test.go).
#[test]
fn shared_prompt_key_caps_stable_aliases_by_recency() {
    let now = base();
    let mut cache = SessionCache::new(Duration::from_secs(60));
    let prompt_key = "openai::pck:shared-cache-bucket::gpt-test";
    for index in 0..128 {
        let conversation = format!("openai::conv:conversation-{index:03}::gpt-test");
        cache.set_aliases("auth-a", &[prompt_key, &conversation], now);
    }
    assert!(
        cache.len() <= 65,
        "{} keys, want one prompt key and at most 64 others",
        cache.len()
    );
    assert!(
        cache.contains("openai::conv:conversation-127::gpt-test"),
        "the newest conversation alias was dropped"
    );
    assert!(
        !cache.contains("openai::conv:conversation-000::gpt-test"),
        "the oldest conversation alias was kept past the cap"
    );
}

// TestSessionCacheRotatingPrimaryEvictsObsoleteAliases (selector_test.go).
#[test]
fn rotating_primary_evicts_obsolete_aliases() {
    let now = base();
    let mut cache = SessionCache::new(Duration::from_secs(60));
    let fallback = "openai::conv:conversation-session::gpt-test";
    for index in 0..16 {
        let primary = format!("openai::pck:cache-{index:02}::gpt-test");
        cache.set_aliases("auth-a", &[&primary, fallback], now);
    }
    let latest = "openai::pck:cache-15::gpt-test";
    let oldest = "openai::pck:cache-00::gpt-test";
    assert_eq!(cache.len(), 2, "want only the latest primary and fallback");
    assert!(cache.contains(latest), "the latest primary was dropped");
    assert!(cache.contains(fallback), "the fallback was dropped");
    assert!(!cache.contains(oldest), "an obsolete primary was kept");
    assert_eq!(cache.group_len(fallback), 2);
}

// TestSessionCache_GetAndRefresh (selector_test.go), with the time moved
// on where upstream sleeps.
#[test]
fn get_and_refresh() {
    let mut cache = SessionCache::new(Duration::from_millis(100));
    cache.set("session1", "auth1", at(0));
    assert_eq!(
        cache.get_and_refresh("session1", at(0)).as_deref(),
        Some("auth1")
    );
    // Half a TTL on, the binding is refreshed.
    assert_eq!(
        cache.get_and_refresh("session1", at(60)).as_deref(),
        Some("auth1")
    );
    // 120ms after it was bound, but 60ms after the refresh.
    assert_eq!(
        cache.get_and_refresh("session1", at(120)).as_deref(),
        Some("auth1"),
        "the refresh should have extended the TTL"
    );
    // A full TTL without use.
    assert_eq!(cache.get_and_refresh("session1", at(230)), None);
}

// TestSessionAffinityAtomicCompareAndDeleteProtectsReboundSession
// (session_affinity_metadata_test.go).
#[test]
fn compare_and_delete_protects_rebound_session() {
    let now = base();
    let mut cache = SessionCache::new(HOUR);
    let key = "mixed::sess-rebound::model-x";

    cache.set(key, "auth-A", now);
    assert_eq!(cache.get(key, now).as_deref(), Some("auth-A"));

    // The session moves to auth-B.
    cache.set(key, "auth-B", now);
    assert_eq!(cache.get(key, now).as_deref(), Some("auth-B"));

    // A stale failure of auth-A leaves it.
    assert!(!cache.compare_and_delete(key, "auth-A", now));
    assert_eq!(cache.get(key, now).as_deref(), Some("auth-B"));

    // A failure of auth-B drops it.
    assert!(cache.compare_and_delete(key, "auth-B", now));
    assert_eq!(cache.get(key, now), None);
}

// TestSessionCacheCompareAndDeleteMultipleAliases (selector_lcp_test.go).
#[test]
fn compare_and_delete_multiple_aliases() {
    let now = base();
    let mut cache = SessionCache::new(HOUR);
    cache.set_aliases("auth-1", &["s1", "s2", "s3"], now);
    for key in ["s1", "s2", "s3"] {
        assert_eq!(cache.get(key, now).as_deref(), Some("auth-1"), "{key}");
    }

    assert!(cache.compare_and_delete("s1", "auth-1", now));
    assert_eq!(cache.get("s1", now), None, "s1 survived");
    for key in ["s2", "s3"] {
        assert_eq!(cache.get(key, now).as_deref(), Some("auth-1"), "{key}");
    }
}

// TestSessionCacheTinyTTLNoPanic (selector_lcp_test.go).
#[test]
fn tiny_ttl_no_panic() {
    let mut cache = SessionCache::new(Duration::from_nanos(1));
    cache.set("test-key", "auth-1", base());
    cache.touch("test-key", "auth-1", base());
    cache.touch("test-key", "auth-1", at(1));
}

// Not upstream's: a binding lives for the TTL after it was last bound or
// touched, and the sweep drops the expired ones a call doesn't reach.
#[test]
fn bindings_expire_and_are_swept() {
    let mut cache = SessionCache::new(Duration::from_millis(100));
    cache.set("kept", "auth-1", at(0));
    cache.set("dropped", "auth-2", at(0));
    assert!(cache.touch("kept", "auth-1", at(80)));
    assert!(!cache.touch("kept", "auth-2", at(80)), "wrong credential");
    assert_eq!(cache.get("kept", at(150)).as_deref(), Some("auth-1"));
    // The sweep at 150ms dropped the other without a call naming it.
    assert!(!cache.contains("dropped"));
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.get("kept", at(180)), None);
    assert!(!cache.touch("kept", "auth-1", at(180)));
}

// Not upstream's: dropping a credential drops every session bound to it
// and no other.
#[test]
fn invalidate_auth_drops_its_sessions() {
    let now = base();
    let mut cache = SessionCache::new(HOUR);
    cache.set_aliases("auth-1", &["s1-a", "s1-b"], now);
    cache.set("s2", "auth-1", now);
    cache.set("s3", "auth-2", now);
    cache.invalidate_auth("auth-1");
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.get("s1-a", now), None);
    assert_eq!(cache.get("s2", now), None);
    assert_eq!(cache.get("s3", now).as_deref(), Some("auth-2"));
}

// Not upstream's: rebinding a session moves all its keys together.
#[test]
fn set_aliases_moves_the_whole_session() {
    let now = base();
    let mut cache = SessionCache::new(HOUR);
    cache.set_aliases("auth-1", &["primary", "parent"], now);
    cache.set("parent", "auth-2", now);
    assert_eq!(cache.get("primary", now).as_deref(), Some("auth-2"));
    assert_eq!(cache.get("parent", now).as_deref(), Some("auth-2"));
    assert_eq!(cache.len(), 2);
}

// Not upstream's: the cache's debug output holds no session key.
#[test]
fn debug_output_holds_no_session_key() {
    let now = base();
    let mut cache = SessionCache::new(HOUR);
    cache.set("mixed::header:secret-session-id::model", "auth-1", now);
    let debug = format!("{cache:?}");
    assert!(!debug.contains("secret-session-id"), "{debug}");
    assert!(debug.contains("Key(..)"), "{debug}");
}
