//! Result recording keeps admission, reload ordering and persistence while
//! config keys avoid a trip through the blocking pool.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use serde_json::json;

use super::cooldown_state_store::{failure, success};
use super::support::*;
use crate::auth::Auth;
use crate::gate::{ActiveGate, Operation};
use crate::manager::{Settings, lock};

fn config_key(id: &str) -> Auth {
    let mut auth = auth_with_metadata(id, "claude", json!({"type": "claude"}));
    auth.attributes.insert("api_key".into(), "test-key".into());
    auth.attributes
        .insert("source".into(), "config:claude[test]".into());
    auth
}

#[test]
fn no_runtime_result_keeps_synchronous_admission_and_persistence() {
    use futures_util::FutureExt;

    let h = Harness::with_store(Settings::default());
    h.manager.register_unsaved(config_key("key")).unwrap();
    h.manager
        .register_unsaved(auth_with_metadata(
            "oauth",
            "claude",
            json!({"type":"claude"}),
        ))
        .unwrap();
    let gate = ActiveGate::open();
    h.manager.set_gate(gate.clone());
    gate.quiesce(0).unwrap();
    // run_persistence historically ran inline without runtime admission;
    // the ineligible key still counts even when no operation can be begun.
    assert!(
        h.manager
            .mark_call_result_async(&success("key", "claude", "m"), None)
            .now_or_never()
            .is_some()
    );
    assert_eq!(h.get("key").success, 1);
    h.manager.set_gate(ActiveGate::open());
    assert!(
        h.manager
            .mark_call_result_async(&success("oauth", "claude", "m"), None)
            .now_or_never()
            .is_some()
    );
    assert_eq!(h.store.stored("oauth").unwrap().success, 1);
}

#[tokio::test(start_paused = true)]
async fn config_key_result_never_uses_the_blocking_path() {
    let h = Harness::with_store(Settings::default());
    h.manager
        .register_unsaved(config_key("result-key"))
        .unwrap();
    let thread = std::thread::current().id();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let manager = h.manager.clone();
    *lock(&h.manager.shared.persist_hook) = Some(Arc::new(move |auth| {
        assert_eq!(std::thread::current().id(), thread);
        assert!(Operation::current_blocking().is_none());
        assert!(Operation::current().is_some(), "result remains admitted");
        assert_eq!(manager.lock().pending_saves.get(&auth.id), Some(&1));
        seen.fetch_add(1, Ordering::SeqCst);
    }));
    h.manager
        .mark_call_result_async(&success("result-key", "claude", "m"), None)
        .await;
    assert_eq!(h.get("result-key").success, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(h.store.save_count(), 0);
    assert!(h.manager.lock().pending_saves.is_empty());
    assert_eq!(h.manager.lock().save_publications, 1);
}

#[tokio::test(start_paused = true)]
async fn persistable_result_uses_the_blocking_path_and_saves() {
    let h = Harness::with_store(Settings::default());
    let auth = auth_with_metadata(
        "result-oauth",
        "claude",
        json!({"type":"claude", "access_token":"test-token"}),
    );
    h.manager.register_unsaved(auth).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let manager = h.manager.clone();
    *lock(&h.manager.shared.persist_hook) = Some(Arc::new(move |auth| {
        assert_eq!(manager.lock().pending_saves.get(&auth.id), Some(&1));
        if Operation::current_blocking().is_some() {
            seen.fetch_add(1, Ordering::SeqCst);
        }
    }));
    h.manager
        .mark_call_result_async(&success("result-oauth", "claude", "m"), None)
        .await;
    assert_eq!(h.store.save_count(), 1, "OAuth result must still persist");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "OAuth persistence must use the blocking path"
    );
    assert_eq!(h.store.stored("result-oauth").unwrap().success, 1);
    assert!(h.manager.lock().pending_saves.is_empty());
}

#[tokio::test(start_paused = true)]
async fn fast_result_matches_counters_cooldowns_and_projections() {
    let fast = Harness::with_store(Settings::default());
    let slow = Harness::with_store(Settings::default());
    fast.add(config_key("key"), &["m"]);
    slow.add(
        auth_with_metadata("key", "claude", json!({"type":"claude"})),
        &["m"],
    );
    for result in [
        failure("key", "claude", "m", 429, "quota"),
        success("key", "claude", "m"),
    ] {
        fast.manager.mark_call_result_async(&result, None).await;
        slow.manager.mark_call_result_async(&result, None).await;
        let a = fast.get("key");
        let b = slow.get("key");
        assert_eq!(
            (a.success, a.failed, a.generation),
            (b.success, b.failed, b.generation)
        );
        assert_eq!(a.success + a.failed, if result.success { 2 } else { 1 });
        assert_eq!(a.model_states, b.model_states);
        assert_eq!(a.recent_requests, b.recent_requests);
        assert_eq!(a.unavailable, b.unavailable);
        assert_eq!(a.next_retry_after, b.next_retry_after);
        assert_eq!(a.quota, b.quota);
        assert_eq!(fast.models.published().len(), slow.models.published().len());
    }
    assert!(
        !fast.get("key").model_states["m"].unavailable,
        "success clears cooldown"
    );
}

#[tokio::test(start_paused = true)]
async fn config_result_refreshes_and_clears_session_affinity() {
    use crate::manager::affinity::Affinity;
    use std::collections::HashSet;

    let h = Harness::with_store(Settings::default());
    let mut key = config_key("key");
    key.provider = "codex".into();
    h.manager.register_unsaved(key).unwrap();
    let session = super::affinity::session(&[("Session-Id", "result-session")], "");
    let key = h.get("key");
    let mut affinity = Affinity::new(Duration::from_secs(60), true);
    affinity
        .pick(
            "mixed",
            "m",
            Some(&session),
            (&[&key], &HashSet::new()),
            h.now(),
            |available| Ok(available[0]),
        )
        .unwrap();
    h.manager.lock().affinity = Some(affinity);
    // Codex config keys must not detach an ineligible persistence tail either.
    *lock(&h.manager.shared.persist_hook) = Some(Arc::new(|_| {
        assert!(Operation::current_blocking().is_none());
    }));
    h.clock.advance(Duration::from_secs(50));
    h.manager
        .mark_call_result_async(&success("key", "codex", "m"), Some(&session))
        .await;
    h.clock.advance(Duration::from_secs(20));
    let cache_key = format!("mixed::{}::m", session.primary());
    assert_eq!(
        h.manager
            .lock()
            .affinity
            .as_mut()
            .unwrap()
            .bound(&cache_key, h.now())
            .as_deref(),
        Some("key"),
        "success extends TTL"
    );
    h.manager
        .mark_call_result_async(&failure("key", "codex", "m", 429, "quota"), Some(&session))
        .await;
    assert_eq!(
        h.manager
            .lock()
            .affinity
            .as_mut()
            .unwrap()
            .bound(&cache_key, h.now()),
        None
    );
}

#[tokio::test]
async fn queued_reload_precedes_result_without_blocking_the_runtime() {
    let h = Harness::with_store(Settings::default());
    h.manager.register_unsaved(config_key("key")).unwrap();
    let mut replacement = config_key("key");
    replacement.success = 40;
    h.store.put(replacement);
    // Hold a mutation so load queues before result recording is polled.
    let mutation = h.manager.mutation_gate();
    let manager = h.manager.clone();
    let reload = std::thread::spawn(move || manager.load().unwrap());
    tokio::time::timeout(Duration::from_secs(10), async {
        while lock(&h.manager.shared.load_gate.state).waiting_exclusive == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let outcome = success("key", "claude", "m");
    let mut recording = Box::pin(h.manager.mark_call_result_async(&outcome, None));
    assert!(
        futures_util::poll!(recording.as_mut()).is_pending(),
        "queued reload takes the blocking fallback"
    );
    assert_eq!(h.get("key").success, 0, "result cannot overtake reload");
    drop(mutation);
    tokio::time::timeout(Duration::from_secs(10), recording)
        .await
        .unwrap();
    reload.join().unwrap();
    assert_eq!(
        h.get("key").success,
        41,
        "result applies to the reloaded registration"
    );
    assert!(h.manager.lock().pending_saves.is_empty());
}

#[tokio::test]
async fn result_fast_path_preserves_drain_admission() {
    let h = Harness::with_store(Settings::default());
    h.manager.register_unsaved(config_key("key")).unwrap();
    let gate = ActiveGate::open();
    h.manager.set_gate(gate.clone());
    let operation = gate.begin_operation().unwrap();
    gate.quiesce(0).unwrap();
    let outcome = success("key", "claude", "m");
    h.manager.mark_call_result_async(&outcome, None).await;
    assert_eq!(
        h.get("key").success,
        0,
        "unscoped draining caller is refused"
    );
    operation
        .scope(h.manager.mark_call_result_async(&outcome, None))
        .await;
    assert_eq!(
        h.get("key").success,
        1,
        "admitted result finishes during drain"
    );
    tokio::time::timeout(Duration::from_secs(10), gate.settle(0))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn neutral_config_result_counts_without_affecting_cooldown() {
    let h = Harness::with_store(Settings::default());
    h.manager.register_unsaved(config_key("key")).unwrap();
    h.manager
        .mark_result(&failure("key", "claude", "m", 429, "quota"));
    let before = h.get("key").model_states.clone();
    let thread = std::thread::current().id();
    *lock(&h.manager.shared.persist_hook) = Some(Arc::new(move |_| {
        assert_eq!(std::thread::current().id(), thread);
        assert!(Operation::current_blocking().is_none());
    }));
    h.manager
        .record_availability_neutral_result_async(&success("key", "claude", "m"))
        .await;
    assert_eq!((h.get("key").success, h.get("key").failed), (1, 1));
    assert_eq!(h.get("key").model_states, before);
}
