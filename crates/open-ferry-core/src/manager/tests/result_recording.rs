//! Result recording keeps admission, reload ordering and persistence while
//! config keys avoid a trip through the blocking pool.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use serde_json::json;

use super::cooldown_state_store::{failure, success};
use super::support::*;
use crate::auth::Auth;
use crate::gate::{ActiveGate, Operation};
use crate::manager::{CallResult, Manager, Settings, lock};

const DEADLINE: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl tracing::Subscriber for LogCapture {
    fn enabled(&self, meta: &tracing::Metadata<'_>) -> bool {
        *meta.level() <= tracing::Level::WARN
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        let mut bytes = lock(&self.0);
        bytes.extend_from_slice(message.0.as_bytes());
        bytes.push(b'\n');
    }
}

/// Poll on a separate runtime thread: a broken synchronous admission must
/// time out on this control thread, rather than block a Tokio timeout too.
struct Recording {
    first: mpsc::Receiver<bool>,
    done: mpsc::Receiver<()>,
    thread: std::thread::JoinHandle<()>,
    logs: Arc<Mutex<Vec<u8>>>,
}

impl Recording {
    fn start(manager: &Manager, result: &CallResult, neutral: bool, cancel: bool) -> Self {
        let manager = manager.clone();
        let result = result.clone();
        let (first_tx, first) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let logs = Arc::new(Mutex::new(Vec::new()));
        let writer = logs.clone();
        let thread = std::thread::spawn(move || {
            crate::test_tracing::keep_every_callsite_open();
            let subscriber = LogCapture(writer);
            tracing::subscriber::with_default(subscriber, || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    let mut future = Box::pin(async {
                        if neutral {
                            manager
                                .record_availability_neutral_result_async(&result)
                                .await;
                        } else {
                            manager.mark_call_result_async(&result, None).await;
                        }
                    });
                    let pending = futures_util::poll!(future.as_mut()).is_pending();
                    first_tx.send(pending).unwrap();
                    if pending && !cancel {
                        future.await;
                    }
                });
                // Runtime shutdown also waits for an uncancelled blocking tail.
                drop(runtime);
                done_tx.send(()).unwrap();
            });
        });
        Self {
            first,
            done,
            thread,
            logs,
        }
    }

    fn first_pending(&self) -> bool {
        self.first
            .recv_timeout(DEADLINE)
            .expect("first poll completes without blocking admission")
    }

    fn finish(self) -> String {
        self.done
            .recv_timeout(DEADLINE)
            .expect("result and its blocking tail finish");
        self.thread.join().unwrap();
        String::from_utf8(lock(&self.logs).clone()).unwrap()
    }
}

fn until(mut ready: impl FnMut() -> bool) {
    let end = Instant::now() + DEADLINE;
    while !ready() {
        assert!(
            Instant::now() < end,
            "synchronization acknowledged within the bound"
        );
        std::thread::yield_now();
    }
}

fn fixed_harness() -> Harness {
    let mut h = Harness::with_store(Settings::default());
    let now = chrono::DateTime::from_timestamp(1_780_272_000, 0).unwrap();
    h.manager = Manager::with_clock(
        Settings::default(),
        h.models.clone(),
        Some(h.store.clone()),
        Arc::new(move || now),
    );
    h
}

/// Arm an existing clock seam once, after mutation admission but before its
/// state-lock attempt. No test-only production path or extra await is needed.
struct ClockPause {
    entered: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
    armed: Arc<Mutex<Option<crate::manager::AdoptHook>>>,
    hook: Option<crate::manager::AdoptHook>,
}

impl ClockPause {
    fn arm(&mut self) {
        *lock(&self.armed) = self.hook.take();
    }
}

fn clock_pause(h: &mut Harness) -> ClockPause {
    let (entered_tx, entered) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let hook: Arc<Mutex<Option<crate::manager::AdoptHook>>> = Arc::default();
    let armed = hook.clone();
    let now = h.now();
    h.manager = Manager::with_clock(
        Settings::default(),
        h.models.clone(),
        Some(h.store.clone()),
        Arc::new(move || {
            let run = lock(&armed).take();
            if let Some(run) = run {
                run();
            }
            now
        }),
    );
    let run: crate::manager::AdoptHook = Arc::new(move || {
        entered_tx.send(()).unwrap();
        lock(&release_rx)
            .recv_timeout(DEADLINE)
            .expect("clock pause released");
    });
    // Return it separately so setup can register before arming the pause.
    ClockPause {
        entered,
        release,
        armed: hook,
        hook: Some(run),
    }
}

fn record(manager: &Manager, result: &CallResult, force_fallback: bool) -> String {
    if force_fallback {
        let state = manager.lock();
        let call = Recording::start(manager, result, false, false);
        assert!(call.first_pending(), "contended state admission falls back");
        drop(state);
        call.finish()
    } else {
        let call = Recording::start(manager, result, false, false);
        assert!(
            !call.first_pending(),
            "uncontended config result finishes inline"
        );
        call.finish()
    }
}

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

#[test]
fn fast_result_matches_counters_cooldowns_and_projections() {
    let fast = fixed_harness();
    let slow = fixed_harness();
    // Both records are config API keys; only admission chooses the path.
    fast.add(config_key("key"), &["m"]);
    slow.add(config_key("key"), &["m"]);
    for result in [
        failure("key", "claude", "m", 429, "quota"),
        success("key", "claude", "m"),
    ] {
        record(&fast.manager, &result, false);
        record(&slow.manager, &result, true);
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
        let projections = |h: &Harness| {
            h.models
                .published()
                .into_iter()
                .map(|p| (p.client_id, p.epoch, p.generation, p.projections))
                .collect::<Vec<_>>()
        };
        assert_eq!(projections(&fast), projections(&slow));
        assert_eq!(
            fast.manager.lock().save_sequence,
            slow.manager.lock().save_sequence
        );
        assert_eq!(
            fast.manager.lock().save_publications,
            slow.manager.lock().save_publications
        );
        assert!(fast.manager.lock().pending_saves.is_empty());
        assert!(slow.manager.lock().pending_saves.is_empty());
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
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    *lock(&h.manager.shared.persist_hook) = Some(Arc::new(move |_| {
        assert!(Operation::current_blocking().is_none());
        seen.fetch_add(1, Ordering::SeqCst);
    }));
    h.clock.advance(Duration::from_secs(50));
    h.manager
        .mark_call_result_async(&success("key", "codex", "m"), Some(&session))
        .await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(h.manager.lock().pending_saves.is_empty());
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
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(h.manager.lock().pending_saves.is_empty());
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

#[test]
fn queued_reload_precedes_result_without_blocking_the_runtime() {
    let h = fixed_harness();
    h.manager.register_unsaved(config_key("key")).unwrap();
    let mut replacement = config_key("key");
    replacement.success = 40;
    h.store.put(replacement);
    let mutation = h.manager.mutation_gate();
    let manager = h.manager.clone();
    let (done_tx, done) = mpsc::channel();
    let reload = std::thread::spawn(move || {
        manager.load().unwrap();
        done_tx.send(()).unwrap();
    });
    until(|| lock(&h.manager.shared.load_gate.state).waiting_exclusive != 0);
    let call = Recording::start(&h.manager, &success("key", "claude", "m"), false, false);
    assert!(
        call.first_pending(),
        "queued reload takes the blocking fallback"
    );
    assert_eq!(h.get("key").success, 0, "result cannot overtake reload");
    drop(mutation);
    call.finish();
    done.recv_timeout(DEADLINE).expect("reload completes");
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
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    *lock(&h.manager.shared.persist_hook) = Some(Arc::new(move |_| {
        assert_eq!(std::thread::current().id(), thread);
        assert!(Operation::current_blocking().is_none());
        seen.fetch_add(1, Ordering::SeqCst);
    }));
    h.manager
        .record_availability_neutral_result_async(&success("key", "claude", "m"))
        .await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(h.manager.lock().pending_saves.is_empty());
    assert_eq!((h.get("key").success, h.get("key").failed), (1, 1));
    assert_eq!(h.get("key").model_states, before);
}

#[test]
fn reload_queued_after_fast_admission_follows_exactly_one_result() {
    let mut h = fixed_harness();
    let mut pause = clock_pause(&mut h);
    let original = h.manager.register_unsaved(config_key("key")).unwrap();
    let mut replacement = config_key("key");
    replacement.success = 40;
    h.store.put(replacement);
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    *lock(&h.manager.shared.persist_hook) = Some(Arc::new(move |auth| {
        assert_eq!((auth.registration_epoch, auth.success), (1, 1));
        assert!(Operation::current_blocking().is_none());
        seen.fetch_add(1, Ordering::SeqCst);
    }));
    let mut result = success("key", "claude", "m");
    result.registration_epoch = original.registration_epoch;
    result.credential_version = original.credential_version;
    pause.arm();
    let call = Recording::start(&h.manager, &result, false, false);
    pause.entered.recv_timeout(DEADLINE).unwrap();
    let manager = h.manager.clone();
    let (done_tx, done) = mpsc::channel();
    let reload = std::thread::spawn(move || {
        manager.load().unwrap();
        done_tx.send(()).unwrap();
    });
    until(|| lock(&h.manager.shared.load_gate.state).waiting_exclusive != 0);
    pause.release.send(()).unwrap();
    assert!(
        !call.first_pending(),
        "admitted result does not reacquire the barrier"
    );
    call.finish();
    done.recv_timeout(DEADLINE)
        .expect("queued reload completes");
    reload.join().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        (h.get("key").registration_epoch, h.get("key").success),
        (2, 40)
    );
    assert!(h.manager.lock().pending_saves.is_empty());
    assert_eq!(lock(&h.manager.shared.load_gate.state).shared, 0);
}

#[test]
fn reload_epoch_fences_captured_results_on_both_paths() {
    for fallback in [false, true] {
        let h = fixed_harness();
        let original = h.manager.register_unsaved(config_key("key")).unwrap();
        let mut result = success("key", "claude", "m");
        result.registration_epoch = original.registration_epoch;
        result.credential_version = original.credential_version;
        let mut replacement = config_key("key");
        replacement.success = 40;
        h.store.put(replacement);
        h.manager.load().unwrap();
        assert_eq!(h.get("key").registration_epoch, 2);
        let before = h.get("key");
        let sequence = h.manager.lock().save_sequence.clone();
        record(&h.manager, &result, fallback);
        let after = h.get("key");
        assert_eq!((after.success, after.generation), (40, before.generation));
        assert_eq!(after.model_states, before.model_states);
        assert_eq!(h.manager.lock().save_sequence, sequence);
        assert!(h.manager.lock().pending_saves.is_empty());
        assert_eq!(h.store.save_count(), 0);
    }
}

#[test]
fn an_edit_to_persistable_before_state_admission_still_saves_the_result() {
    let mut h = fixed_harness();
    let mut pause = clock_pause(&mut h);
    let original = h.manager.register_unsaved(config_key("key")).unwrap();
    let mut result = success("key", "claude", "m");
    result.registration_epoch = original.registration_epoch;
    result.credential_version = original.credential_version;
    pause.arm();
    let call = Recording::start(&h.manager, &result, false, false);
    pause.entered.recv_timeout(DEADLINE).unwrap();
    h.manager
        .edit_unsaved("key", |auth| {
            Ok::<_, ()>(auth.attributes.remove("source").is_some())
        })
        .unwrap();
    assert_eq!(h.get("key").credential_version, original.credential_version);
    assert!(!h.manager.permanently_ineligible(&h.get("key")));
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    *lock(&h.manager.shared.persist_hook) = Some(Arc::new(move |_| {
        assert!(Operation::current_blocking().is_some());
        seen.fetch_add(1, Ordering::SeqCst);
    }));
    // Unsaved edits also visit the persistence seam; count the result alone.
    calls.store(0, Ordering::SeqCst);
    pause.release.send(()).unwrap();
    call.first_pending();
    call.finish();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(h.store.save_count(), 1);
    assert_eq!(h.store.stored("key").unwrap().success, 1);
    assert!(h.manager.lock().pending_saves.is_empty());
}

#[test]
fn contended_state_admission_falls_back_without_waiting() {
    let h = fixed_harness();
    h.manager.register_unsaved(config_key("key")).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    *lock(&h.manager.shared.persist_hook) = Some(Arc::new(move |_| {
        assert!(Operation::current_blocking().is_some());
        seen.fetch_add(1, Ordering::SeqCst);
    }));
    record(&h.manager, &success("key", "claude", "m"), true);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(h.get("key").success, 1);
}

#[test]
fn cancelling_the_blocking_fallback_keeps_its_admission_and_result() {
    use futures_util::FutureExt;
    let h = fixed_harness();
    h.manager.register_unsaved(config_key("key")).unwrap();
    let gate = ActiveGate::open();
    h.manager.set_gate(gate.clone());
    let state = h.manager.lock();
    let call = Recording::start(&h.manager, &success("key", "claude", "m"), false, true);
    assert!(call.first_pending());
    gate.quiesce(0).unwrap();
    drop(state);
    call.finish();
    assert_eq!(h.get("key").success, 1);
    assert!(h.manager.lock().pending_saves.is_empty());
    assert!(gate.settle(0).now_or_never().unwrap().is_ok());
}

#[test]
fn inline_panics_match_blocking_warnings_poison_and_cleanup() {
    let mut warnings = Vec::new();
    for fallback in [false, true] {
        let h = fixed_harness();
        h.manager.register_unsaved(config_key("key")).unwrap();
        let manager = h.manager.clone();
        *lock(&h.manager.shared.persist_hook) = Some(Arc::new(move |_| {
            let _state = manager.lock();
            panic!("injected result panic with the state lock held");
        }));
        warnings.push(record(&h.manager, &success("key", "claude", "m"), fallback));
        assert!(h.manager.shared.state.is_poisoned());
        assert_eq!(
            h.get("key").success,
            1,
            "poison recovery keeps the recorded state"
        );
        assert!(h.manager.lock().pending_saves.is_empty());
        assert_eq!(lock(&h.manager.shared.load_gate.state).shared, 0);
    }
    assert!(warnings[0].contains("record call result failed:"));
    assert!(warnings[0].contains("persistence task failed"));
    assert_eq!(warnings[0], warnings[1]);
}

#[tokio::test]
async fn synchronous_codex_results_keep_the_drain_refusal_warning() {
    let h = fixed_harness();
    let mut auth = config_key("key");
    auth.provider = "codex".into();
    h.manager.register_unsaved(auth).unwrap();
    let gate = ActiveGate::open();
    h.manager.set_gate(gate.clone());
    gate.quiesce(0).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    *lock(&h.manager.shared.persist_hook) = Some(Arc::new(move |_| {
        seen.fetch_add(1, Ordering::SeqCst);
    }));
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer = logs.clone();
    crate::test_tracing::keep_every_callsite_open();
    let subscriber = LogCapture(writer);
    tracing::subscriber::with_default(subscriber, || {
        h.manager.mark_result(&success("key", "codex", "m"))
    });
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "synchronous ineligible tail remains refused"
    );
    assert!(
        String::from_utf8(lock(&logs).clone())
            .unwrap()
            .contains("credential state save refused: operation gate is not open")
    );
    assert!(h.manager.lock().pending_saves.is_empty());
}
