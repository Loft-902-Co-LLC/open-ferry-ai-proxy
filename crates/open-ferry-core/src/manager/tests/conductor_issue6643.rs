// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_issue6643_test.go (v8.0.32, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A success from a call dispatched before a cooldown was recorded doesn't
//! clear that cooldown: only a success dispatched after it proves recovery.
//!
//! Deviations from upstream:
//! - Upstream's tests use its `antigravity` provider, which isn't ported;
//!   these use `claude`.
//! - `TestManager_Issue6643_InFlightSuccessesKeepFreshQuotaCooldown` waits
//!   for the quota failure to be recorded by polling the credential, as
//!   there is no result hook to release the blocked calls from.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_core::future::BoxFuture;
use tokio::sync::Notify;

use super::cooldown_state_store::RecordingStore;
use super::support::*;
use crate::auth::{Auth, AuthError, ModelState, Status};
use crate::exec::{Dispatcher, ExecError, Options, Request, Response};
use crate::executor::ProviderExecutor;
use crate::manager::cooldown_store::{flush, install_store, restore_now, set_debounce};
use crate::manager::select::is_auth_blocked_for_model;
use crate::manager::{CallResult, Settings};

const MODEL: &str = "claude-opus-5-5";
const PROVIDER: &str = "claude";

fn quota_error() -> AuthError {
    AuthError {
        code: "quota_exhausted".into(),
        message: "QUOTA_EXHAUSTED".into(),
        retryable: true,
        http_status: 429,
    }
}

fn quota_failure(id: &str, model: &str, execution_seq: u64) -> CallResult {
    CallResult {
        auth_id: id.into(),
        provider: PROVIDER.into(),
        model: model.into(),
        success: false,
        execution_seq,
        error: Some(quota_error()),
        ..CallResult::default()
    }
}

fn success(id: &str, model: &str, execution_seq: u64) -> CallResult {
    CallResult {
        auth_id: id.into(),
        provider: PROVIDER.into(),
        model: model.into(),
        success: true,
        execution_seq,
        ..CallResult::default()
    }
}

fn harness_with(id: &str) -> Harness {
    let h = Harness::new(Settings::default());
    let mut candidate = auth(id, PROVIDER);
    candidate.status = Status::Active;
    h.manager
        .register_unsaved(candidate)
        .expect("register the credential");
    h
}

fn model_state(h: &Harness, id: &str, model: &str) -> ModelState {
    h.get(id)
        .model_states
        .get(model)
        .cloned()
        .unwrap_or_else(|| panic!("expected model state for {model}"))
}

/// Upstream's TestManager_Issue6643_SuccessDispatchedAfterCooldownClearsIt:
/// only a success stamped after the cooldown was recorded clears it.
#[tokio::test(start_paused = true)]
async fn success_dispatched_after_cooldown_clears_it() {
    let id = "issue6643-after-auth";
    let h = harness_with(id);

    // A long-running call is dispatched first, then a later dispatch hits the
    // quota limit and records the cooldown.
    let stale = h.manager.next_execution_seq();
    let failed = h.manager.next_execution_seq();
    h.manager.mark_result(&quota_failure(id, MODEL, failed));
    let state = model_state(&h, id, MODEL);
    assert!(state.quota.exceeded, "expected a quota cooldown: {state:?}");

    // The earlier dispatch now succeeds. It predates the cooldown, so the
    // cooldown survives.
    h.manager.mark_result(&success(id, MODEL, stale));
    let state = model_state(&h, id, MODEL);
    assert!(
        state.unavailable
            && state.quota.exceeded
            && state.next_retry_after.is_some_and(|t| t > h.now()),
        "stale success cleared the quota cooldown: {state:?}"
    );

    // A call dispatched after the cooldown was recorded succeeds and clears it.
    let fresh = h.manager.next_execution_seq();
    h.manager.mark_result(&success(id, MODEL, fresh));
    let state = model_state(&h, id, MODEL);
    assert!(
        !state.unavailable && !state.quota.exceeded && state.next_retry_after.is_none(),
        "post-cooldown success did not clear the cooldown: {state:?}"
    );
}

/// Upstream's TestManager_Issue6643_UnstampedSuccessKeepsLegacyClearBehavior:
/// a result with no dispatch stamp can't be ordered against the cooldown, so
/// it clears it as before.
#[tokio::test(start_paused = true)]
async fn unstamped_success_keeps_legacy_clear_behavior() {
    let id = "issue6643-unstamped-auth";
    let h = harness_with(id);

    let failed = h.manager.next_execution_seq();
    h.manager.mark_result(&quota_failure(id, MODEL, failed));
    assert!(model_state(&h, id, MODEL).quota.exceeded);

    h.manager.mark_result(&success(id, MODEL, 0));
    let state = model_state(&h, id, MODEL);
    assert!(
        !state.unavailable && !state.quota.exceeded,
        "an unstamped success should clear the cooldown: {state:?}"
    );
}

/// Upstream's TestManager_Issue6643_StaleAuthLevelSuccessKeepsCooldown: a
/// success with no model takes the credential-level clear path, and must not
/// wipe a credential-level cooldown recorded after it was dispatched.
#[tokio::test(start_paused = true)]
async fn stale_auth_level_success_keeps_cooldown() {
    let id = "issue6643-auth-level-auth";
    let h = harness_with(id);

    let stale = h.manager.next_execution_seq();
    let failed = h.manager.next_execution_seq();
    h.manager.mark_result(&CallResult {
        auth_id: id.into(),
        provider: PROVIDER.into(),
        success: false,
        execution_seq: failed,
        error: Some(quota_error()),
        ..CallResult::default()
    });
    let current = h.get(id);
    assert!(
        current.quota.exceeded && current.next_retry_after.is_some_and(|t| t > h.now()),
        "expected a credential-level quota cooldown: {:?}",
        current.quota
    );

    h.manager.mark_result(&CallResult {
        auth_id: id.into(),
        provider: PROVIDER.into(),
        success: true,
        execution_seq: stale,
        ..CallResult::default()
    });
    let current = h.get(id);
    assert!(
        current.quota.exceeded && current.next_retry_after.is_some_and(|t| t > h.now()),
        "a stale credential-level success cleared the cooldown: {:?}",
        current.quota
    );

    let fresh = h.manager.next_execution_seq();
    h.manager.mark_result(&CallResult {
        auth_id: id.into(),
        provider: PROVIDER.into(),
        success: true,
        execution_seq: fresh,
        ..CallResult::default()
    });
    let current = h.get(id);
    assert!(
        !current.quota.exceeded && current.next_retry_after.is_none(),
        "a post-cooldown credential-level success did not clear the cooldown: {:?}",
        current.quota
    );
}

/// Upstream's TestManager_Issue6643_RestoreKeepsCooldownWatermark: restoring
/// the saved cooldowns into the live credential keeps the watermark that
/// protects a fresh cooldown from stale in-flight successes.
#[tokio::test(start_paused = true)]
async fn restore_keeps_cooldown_watermark() {
    let id = "issue6643-restore-auth";
    let h = harness_with(id);
    set_debounce(&h.manager, Duration::from_secs(3600));
    let store = Arc::new(RecordingStore::default());
    install_store(&h.manager, store.clone());
    restore_now(&h.manager);

    let stale = h.manager.next_execution_seq();
    let failed = h.manager.next_execution_seq();
    h.manager.mark_result(&quota_failure(id, MODEL, failed));
    assert!(model_state(&h, id, MODEL).quota.exceeded);
    flush(&h.manager);
    let saved = store.saved();
    assert!(!saved.is_empty(), "expected the cooldown to be saved");

    // A configuration update restores the saved cooldown into the live
    // credential while the earlier dispatch is still in flight.
    install_store(&h.manager, RecordingStore::with_load(saved));
    restore_now(&h.manager);

    h.manager.mark_result(&success(id, MODEL, stale));
    let state = model_state(&h, id, MODEL);
    assert!(
        state.unavailable
            && state.quota.exceeded
            && state.next_retry_after.is_some_and(|t| t > h.now()),
        "stale success cleared the cooldown after a restore: {state:?}"
    );
}

/// Not upstream's: a cooldown restored after a restart has no watermark, so
/// any stamped success clears it ("zero after restart" in upstream's field
/// comment).
#[tokio::test(start_paused = true)]
async fn restored_cooldown_without_a_watermark_is_cleared_by_any_success() {
    let id = "issue6643-restart-auth";
    let h = harness_with(id);
    set_debounce(&h.manager, Duration::from_secs(3600));
    let first = Arc::new(RecordingStore::default());
    install_store(&h.manager, first.clone());
    restore_now(&h.manager);
    let failed = h.manager.next_execution_seq();
    h.manager.mark_result(&quota_failure(id, MODEL, failed));
    flush(&h.manager);
    let saved = first.saved();

    // A new process: a fresh manager restores the saved cooldown.
    let restarted = harness_with(id);
    set_debounce(&restarted.manager, Duration::from_secs(3600));
    install_store(&restarted.manager, RecordingStore::with_load(saved));
    restore_now(&restarted.manager);
    assert!(model_state(&restarted, id, MODEL).quota.exceeded);
    assert_eq!(model_state(&restarted, id, MODEL).cooldown_recorded_seq, 0);

    let stamp = restarted.manager.next_execution_seq();
    restarted.manager.mark_result(&success(id, MODEL, stamp));
    let state = model_state(&restarted, id, MODEL);
    assert!(
        !state.unavailable && !state.quota.exceeded,
        "a restored cooldown should be clearable by any success: {state:?}"
    );
}

/// Five calls run on one credential; a later dispatch is answered with a
/// quota 429, and only then do the earlier ones succeed (upstream's
/// `issue6643ConcurrentExecutor`).
struct ConcurrentExecutor {
    dispatched: AtomicUsize,
    after_exhaustion: AtomicUsize,
    quota_recorded: Notify,
}

const CONCURRENT_REQUESTS: usize = 6;

impl ProviderExecutor for ConcurrentExecutor {
    fn id(&self) -> &str {
        PROVIDER
    }

    fn execute(
        &self,
        _auth: Arc<Auth>,
        _request: Request,
        _options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        Box::pin(async move {
            let call = self.dispatched.fetch_add(1, Ordering::SeqCst) + 1;
            if call < CONCURRENT_REQUESTS {
                // Long-running calls dispatched before the quota error: they
                // succeed only once the 429 has been recorded.
                self.quota_recorded.notified().await;
                return Ok(Response {
                    payload: "ok".into(),
                    headers: http::HeaderMap::new(),
                });
            }
            if call > CONCURRENT_REQUESTS {
                self.after_exhaustion.fetch_add(1, Ordering::SeqCst);
            }
            // The last dispatch sees the quota run out right now.
            Err(ExecError::upstream(
                429,
                "QUOTA_EXHAUSTED: Resets in 140h36m",
            ))
        })
    }

    fn count_tokens(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        self.execute(auth, request, options)
    }

    fn execute_stream(
        &self,
        _auth: Arc<Auth>,
        _request: Request,
        _options: Options,
    ) -> BoxFuture<'_, Result<crate::exec::StreamResponse, ExecError>> {
        Box::pin(async { Err(ExecError::upstream(501, "not implemented")) })
    }

    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        Box::pin(async move { Ok((*auth).clone()) })
    }
}

/// Upstream's TestManager_Issue6643_InFlightSuccessesKeepFreshQuotaCooldown:
/// successes from calls dispatched before a quota cooldown was recorded leave
/// it in place, so the exhausted credential stays out of selection.
#[tokio::test(start_paused = true)]
async fn in_flight_successes_keep_fresh_quota_cooldown() {
    let id = "issue6643-auth";
    let h = Harness::new(Settings::default());
    h.add(auth(id, PROVIDER), &[MODEL]);
    let executor = Arc::new(ConcurrentExecutor {
        dispatched: AtomicUsize::new(0),
        after_exhaustion: AtomicUsize::new(0),
        quota_recorded: Notify::new(),
    });
    h.manager.register_executor(executor.clone());

    let mut tasks = Vec::new();
    for _ in 0..CONCURRENT_REQUESTS {
        let manager = h.manager.clone();
        tasks.push(tokio::spawn(async move {
            manager
                .execute(&providers(&[PROVIDER]), request(MODEL), options())
                .await
        }));
        // Each call is dispatched before the next starts, so the failing one
        // is the last.
        settle().await;
    }
    // Release the blocked calls once the quota failure is recorded.
    while !h
        .get(id)
        .model_states
        .get(MODEL)
        .is_some_and(|s| s.quota.exceeded)
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    executor.quota_recorded.notify_waiters();

    let mut failures = 0;
    for task in tasks {
        if task.await.expect("task").is_err() {
            failures += 1;
        }
    }
    // The client sees exactly one failure: the dispatch that hit the limit.
    assert_eq!(failures, 1, "concurrent failures");
    assert_eq!(
        executor.after_exhaustion.load(Ordering::SeqCst),
        0,
        "upstream calls after quota exhaustion"
    );

    let current = h.get(id);
    let state = current
        .model_states
        .get(MODEL)
        .expect("expected a model state");
    assert!(
        state.unavailable
            && state.quota.exceeded
            && state.next_retry_after.is_some_and(|t| t > h.now()),
        "quota cooldown was cleared by in-flight successes: {state:?}"
    );
    let (blocked, _, _) = is_auth_blocked_for_model(&current, MODEL, h.now());
    assert!(
        blocked,
        "the exhausted credential is selectable again during its quota cooldown"
    );
}
