// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_scheduler_cooldown_rebuild_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Picks while credentials cool down: a cooling model answers with a model
//! cooldown error every time, concurrent picks and results neither block
//! healthy models nor bring a cooled or disabled credential back, and a
//! model the registry adds later is picked at once.
//!
//! Deviations from upstream:
//! - The port has no scheduler index (see the module docs), so there's no
//!   rebuild to avoid. Each test keeps the picks and errors upstream checks
//!   and drops the index-only checks: the provider scheduler's pointer, the
//!   synced and current versions, and `scheduler.rebuild` with an old
//!   snapshot.
//! - `shouldRetrySchedulerPick` (retry after a rebuild) becomes whether the
//!   error is an auth-selection error (`ErrorKind::is_auth_selection`), the
//!   part of it that doesn't depend on versions.
//! - Picks are calls through the manager (`execute`), with an executor
//!   registered for the provider. The exact cooldown errors are upstream's,
//!   taken from running the same picks against upstream.
//! - Goroutines become Tokio tasks on one thread; upstream's 2s and 5s
//!   starvation timeouts become Tokio timeouts in paused time, which fire
//!   only if the tasks stop making progress.
//! - `scheduler_concurrent_picks_one_sync_allows_others_to_retry`: picks
//!   read the registry live, so no pick fails first; every request gets the
//!   credential.
//! - `scheduler_model_projection_cooldown_does_not_invalidate_fast_path`:
//!   upstream's current version is the registration epoch plus the
//!   registry's; this checks the credential's epoch and the registry epoch
//!   the projections were published against.

use super::support::*;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::auth::{AuthError, Status};
use crate::exec::{Dispatcher, ErrorKind, ExecError};
use crate::manager::{CallResult, Manager, Settings};

/// Runs a call for `model` over `provider`.
async fn call(manager: &Manager, provider: &str, model: &str) -> Result<(), ExecError> {
    manager
        .execute(&providers(&[provider]), request(model), options())
        .await
        .map(drop)
}

/// Upstream's 429 "quota exceeded" result for `model` on `auth_id`.
fn quota_exceeded(auth_id: &str, provider: &str, model: &str) -> CallResult {
    CallResult {
        auth_id: auth_id.into(),
        provider: provider.into(),
        model: model.into(),
        success: false,
        error: Some(AuthError {
            http_status: 429,
            message: "quota exceeded".into(),
            ..AuthError::default()
        }),
        ..CallResult::default()
    }
}

/// Asserts `err` is upstream's cooldown error for `model` after
/// [`quota_exceeded`]: the first quota cooldown is one second.
fn assert_cooldown(err: &ExecError, model: &str, provider: &str) {
    assert_eq!(err.kind, ErrorKind::ModelCooldown, "{err}");
    assert_eq!(err.status, 429);
    assert_eq!(err.retry_after, Some(Duration::from_secs(1)));
    assert_eq!(
        err.to_string(),
        format!(
            r#"{{"error":{{"code":"model_cooldown","last_upstream_error":"quota exceeded","message":"All credentials for model {model} are cooling down via provider {provider} (last error: quota exceeded)","model":"{model}","provider":"{provider}","reset_seconds":1,"reset_time":"1s"}}}}"#
        )
    );
}

#[tokio::test(start_paused = true)]
async fn scheduler_model_cooldown_does_not_trigger_rebuild() {
    const MODEL: &str = "scheduler-cooldown-no-rebuild-model";
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::new("gemini");
    h.executor(&executor);
    let id = "auth-cooldown-no-rebuild";
    h.add(auth(id, "gemini"), &[MODEL]);

    let picked = pick_by_call(&h, &executor, "gemini", MODEL)
        .await
        .expect("initial pick");
    assert_eq!(picked, id);

    h.manager.mark_result(&quota_exceeded(id, "gemini", MODEL));

    let err = pick_by_call(&h, &executor, "gemini", MODEL)
        .await
        .expect_err("a cooling credential");
    assert_cooldown(&err, MODEL, "gemini");
    assert!(
        !err.kind.is_auth_selection(),
        "a cooldown must not count as a stale pick"
    );

    let err = pick_by_call(&h, &executor, "gemini", MODEL)
        .await
        .expect_err("pick on a cooled down model");
    assert_cooldown(&err, MODEL, "gemini");

    let err = pick_by_call(&h, &executor, "gemini", MODEL)
        .await
        .expect_err("subsequent pick");
    assert_cooldown(&err, MODEL, "gemini");
    assert_eq!(executor.ids(Kind::Execute), [id]);
}

#[tokio::test(start_paused = true)]
async fn scheduler_concurrent_cooldown_picks_do_not_block_healthy_model() {
    const COOLED: &str = "model-cooling";
    const HEALTHY: &str = "model-healthy";
    const CONCURRENCY: usize = 30;
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::new("gemini");
    h.executor(&executor);
    h.add(auth("auth-cool", "gemini"), &[COOLED]);
    h.add(auth("auth-healthy", "gemini"), &[HEALTHY]);
    h.manager
        .mark_result(&quota_exceeded("auth-cool", "gemini", COOLED));

    let cooled: Vec<_> = (0..CONCURRENCY)
        .map(|_| {
            let manager = h.manager.clone();
            tokio::spawn(async move { call(&manager, "gemini", COOLED).await })
        })
        .collect();
    let manager = h.manager.clone();
    let healthy = tokio::spawn(async move { call(&manager, "gemini", HEALTHY).await });

    tokio::time::timeout(Duration::from_secs(5), healthy)
        .await
        .expect("healthy model pick was blocked/starved by concurrent cooldown picks")
        .expect("the healthy task")
        .expect("healthy pick");
    let results = tokio::time::timeout(
        Duration::from_secs(5),
        futures_util::future::join_all(cooled),
    )
    .await
    .expect("concurrent cooldown pick workers did not complete within timeout");
    for result in results {
        let err = result
            .expect("a cooling task")
            .expect_err("a cooling model");
        assert_cooldown(&err, COOLED, "gemini");
    }
    assert_eq!(executor.ids(Kind::Execute), ["auth-healthy"]);
}

#[tokio::test(start_paused = true)]
async fn scheduler_interleaved_mark_result_does_not_trigger_rebuild() {
    const ACTIVE: &str = "model-active";
    const COOLING: &str = "model-in-cooldown";
    const RESULT_WORKERS: usize = 5;
    const PICK_WORKERS: usize = 20;
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::new("gemini");
    h.executor(&executor);
    h.add(auth("auth-active", "gemini"), &[ACTIVE]);
    h.add(auth("auth-cooled", "gemini"), &[COOLING]);
    h.manager
        .mark_result(&quota_exceeded("auth-cooled", "gemini", COOLING));

    let stop = Arc::new(AtomicBool::new(false));
    let results: Vec<_> = (0..RESULT_WORKERS)
        .map(|_| {
            let manager = h.manager.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                while !stop.load(Ordering::SeqCst) {
                    manager.mark_result(&CallResult {
                        auth_id: "auth-active".into(),
                        provider: "gemini".into(),
                        model: ACTIVE.into(),
                        success: true,
                        ..CallResult::default()
                    });
                    tokio::task::yield_now().await;
                }
            })
        })
        .collect();
    let picks: Vec<_> = (0..PICK_WORKERS)
        .map(|_| {
            let manager = h.manager.clone();
            tokio::spawn(async move {
                let mut errors = Vec::new();
                for _ in 0..10 {
                    errors.push(call(&manager, "gemini", COOLING).await);
                    tokio::task::yield_now().await;
                }
                errors
            })
        })
        .collect();

    let picked = tokio::time::timeout(
        Duration::from_secs(2),
        futures_util::future::join_all(picks),
    )
    .await;
    stop.store(true, Ordering::SeqCst);
    let picked = picked.expect("pick workers timed out while MarkResult was running");
    futures_util::future::join_all(results).await;

    for worker in picked {
        for result in worker.expect("a pick worker") {
            let err = result.expect_err("expected pickNext on cooldown model to fail");
            assert_cooldown(&err, COOLING, "gemini");
        }
    }
    assert!(executor.ids(Kind::Execute).is_empty());
}

#[tokio::test(start_paused = true)]
async fn scheduler_large_credential_set_cooldown_does_not_lock_starve() {
    const CREDENTIALS: usize = 40;
    const CONCURRENT_PICKS: usize = 30;
    const TARGET: &str = "antigravity-cooldown-model";
    const SIBLING: &str = "antigravity-healthy-model";
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::new("antigravity");
    h.executor(&executor);
    for i in 0..CREDENTIALS {
        h.add(
            auth(&format!("ag-auth-{i}"), "antigravity"),
            &[TARGET, SIBLING],
        );
    }

    pick_by_call(&h, &executor, "antigravity", TARGET)
        .await
        .expect("initial targetModel pick");

    for i in 0..CREDENTIALS {
        h.manager.mark_result(&quota_exceeded(
            &format!("ag-auth-{i}"),
            "antigravity",
            TARGET,
        ));
    }

    let err = pick_by_call(&h, &executor, "antigravity", TARGET)
        .await
        .expect_err("every credential cools down for the target model");
    assert_cooldown(&err, TARGET, "antigravity");

    let target: Vec<_> = (0..CONCURRENT_PICKS)
        .map(|_| {
            let manager = h.manager.clone();
            tokio::spawn(async move { call(&manager, "antigravity", TARGET).await })
        })
        .collect();
    let manager = h.manager.clone();
    let sibling = tokio::spawn(async move { call(&manager, "antigravity", SIBLING).await });

    tokio::time::timeout(Duration::from_secs(5), sibling)
        .await
        .expect("healthy sibling model pick was starved by large credential set cooldown picks")
        .expect("the sibling task")
        .expect("sibling model pick");
    let results = tokio::time::timeout(
        Duration::from_secs(5),
        futures_util::future::join_all(target),
    )
    .await
    .expect("concurrent targetModel picks timed out");
    for result in results {
        let err = result
            .expect("a target task")
            .expect_err("expected pickNext on targetModel to fail with cooldown error");
        assert_cooldown(&err, TARGET, "antigravity");
    }
    assert_eq!(executor.models(Kind::Execute), [TARGET, SIBLING]);
}

#[tokio::test(start_paused = true)]
async fn scheduler_rebuild_during_concurrent_mark_result_preserves_latest_state() {
    const MODEL: &str = "model-concurrent-mark-rebuild";
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::new("gemini");
    h.executor(&executor);
    let id = "auth-concurrent-mark";
    h.add(auth(id, "gemini"), &[MODEL]);

    // Case A: a healthy credential cools down.
    h.manager.mark_result(&quota_exceeded(id, "gemini", MODEL));
    let err = pick_by_call(&h, &executor, "gemini", MODEL)
        .await
        .expect_err("the cooldown must hold");
    assert_cooldown(&err, MODEL, "gemini");

    // Case B: the cooled credential recovers.
    h.manager
        .reset_quota(id)
        .expect("reset quota")
        .expect("a registered credential");
    let picked = pick_by_call(&h, &executor, "gemini", MODEL)
        .await
        .expect("the recovery must hold");
    assert_eq!(picked, id);
}

#[tokio::test(start_paused = true)]
async fn scheduler_model_projection_cooldown_does_not_invalidate_fast_path() {
    const MODEL: &str = "model-projection-fastpath";
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::new("gemini");
    h.executor(&executor);
    let id = "auth-proj-fastpath";
    h.add(auth(id, "gemini"), &[MODEL]);
    let (epoch, _) = h.versions(id);

    h.manager.mark_result(&quota_exceeded(id, "gemini", MODEL));

    assert_eq!(
        h.versions(id).0,
        epoch,
        "cooldown must not invalidate the registration epoch"
    );
    let published = h.models.published();
    let last = published.last().expect("the cooldown was published");
    assert_eq!(last.client_id, id);
    assert_eq!(
        last.epoch, 1,
        "published against the registry's first epoch"
    );
    assert!(
        h.models
            .projection(id, MODEL)
            .expect("a projection")
            .suspended
    );

    let err = pick_by_call(&h, &executor, "gemini", MODEL)
        .await
        .expect_err("a cooling credential");
    assert_cooldown(&err, MODEL, "gemini");
    assert!(!err.kind.is_auth_selection());
}

#[tokio::test(start_paused = true)]
async fn scheduler_rebuild_during_concurrent_update_does_not_resurrect_disabled_credential() {
    const MODEL: &str = "model-disabled-rebuild";
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::new("gemini");
    h.executor(&executor);
    let id = "auth-disabled-rebuild";
    let mut active = auth(id, "gemini");
    active.status = Status::Active;
    let active = h.add(active, &[MODEL]);

    let mut disabled = (*active).clone();
    disabled.disabled = true;
    disabled.status = Status::Disabled;
    h.manager
        .update(disabled)
        .expect("update auth to disabled")
        .expect("a registered credential");

    let err = pick_by_call(&h, &executor, "gemini", MODEL)
        .await
        .expect_err("the disabled credential was picked");
    assert_eq!(err.kind, ErrorKind::AuthNotFound);
    assert_eq!(err.to_string(), "auth_not_found: no auth available");
    assert!(executor.ids(Kind::Execute).is_empty());
}

#[tokio::test(start_paused = true)]
async fn scheduler_incremental_refresh_allows_failed_pick_to_retry_and_succeed() {
    const INITIAL: &str = "model-incremental-initial";
    const NEW: &str = "model-incremental-new";
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::new("gemini");
    h.executor(&executor);
    let id = "auth-incremental-test";
    h.add(auth(id, "gemini"), &[INITIAL]);

    let err = pick_by_call(&h, &executor, "gemini", NEW)
        .await
        .expect_err("expected pickSingle on newModel to fail initially");
    assert_eq!(err.to_string(), "auth_not_found: no auth available");
    assert!(err.kind.is_auth_selection(), "the failed pick may retry");

    h.models.register(id, &[INITIAL, NEW]);

    let picked = pick_by_call(&h, &executor, "gemini", NEW)
        .await
        .expect("retry pick");
    assert_eq!(picked, id);
}

#[tokio::test(start_paused = true)]
async fn scheduler_concurrent_picks_one_sync_allows_others_to_retry() {
    const INITIAL: &str = "model-concurrent-init";
    const NEW: &str = "model-concurrent-new";
    const REQUESTS: usize = 10;
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::new("gemini");
    h.executor(&executor);
    let id = "auth-concurrent-retry";
    h.add(auth(id, "gemini"), &[INITIAL]);
    h.models.register(id, &[INITIAL, NEW]);

    let requests: Vec<_> = (0..REQUESTS)
        .map(|_| {
            let manager = h.manager.clone();
            tokio::spawn(async move { call(&manager, "gemini", NEW).await })
        })
        .collect();
    for (i, result) in futures_util::future::join_all(requests)
        .await
        .into_iter()
        .enumerate()
    {
        if let Err(err) = result.expect("a request task") {
            panic!("request {i} failed to discover newly synced model: {err}");
        }
    }
    assert_eq!(executor.ids(Kind::Execute), vec![id; REQUESTS]);
}

#[tokio::test(start_paused = true)]
async fn scheduler_register_client_invalidates_fast_path() {
    const INITIAL: &str = "model-init";
    const DYNAMIC: &str = "model-dynamic";
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::new("gemini");
    h.executor(&executor);
    let id = "auth-dynamic-reg";
    h.add(auth(id, "gemini"), &[INITIAL]);

    h.models.register(id, &[INITIAL, DYNAMIC]);

    let picked = pick_by_call(&h, &executor, "gemini", DYNAMIC)
        .await
        .expect("pickNext failed to discover dynamically registered model");
    assert_eq!(picked, id);
}

#[tokio::test(start_paused = true)]
async fn scheduler_unschedulable_auth_does_not_trigger_rebuild_loop() {
    const MODEL: &str = "model-unschedulable-test";
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::new("gemini");
    h.executor(&executor);
    let id = "auth-valid-cooling";
    h.add(auth(id, "gemini"), &[MODEL]);
    // A credential no provider can schedule.
    h.add(auth("auth-empty-provider", ""), &[]);

    h.manager.mark_result(&quota_exceeded(id, "gemini", MODEL));

    let err = pick_by_call(&h, &executor, "gemini", MODEL)
        .await
        .expect_err("expected pickNext to fail with cooldown error");
    assert_cooldown(&err, MODEL, "gemini");
    for _ in 0..5 {
        let err = pick_by_call(&h, &executor, "gemini", MODEL)
            .await
            .expect_err("a cooling credential");
        assert_cooldown(&err, MODEL, "gemini");
    }
    assert!(executor.ids(Kind::Execute).is_empty());
}
