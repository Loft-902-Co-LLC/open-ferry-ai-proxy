// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_oauth_alias_nofork_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! OAuth model aliases without fork, where a route model B stands for an
//! upstream model A: cooldowns are kept under A, reconciling with the
//! registry keeps, migrates or prunes them, and projections, picks and saves
//! follow. Also the registration epoch checks upstream keeps in this file.
//!
//! Deviations from upstream:
//! - All tests: the registry is a `FakeModels`, so suspension and quota read
//!   the last projection the manager published. `ModelAlias` has no `fork`
//!   field, and upstream's manager doesn't read it either. The cooldown
//!   state store isn't ported (a deviation of the service), and `FileStore`
//!   saves a credential's metadata, not its model states, so a cooldown
//!   doesn't outlive the process. "The store has a record" here means the
//!   credential the manager last handed the `FakeStore` is in active
//!   cooldown: what it would hand a cooldown store. Those credentials carry
//!   metadata so the manager saves them. Picks go through `execute` and
//!   report the credential the executor got (upstream's
//!   `scheduler.pickSingle`).
//! - `cooldown_persistence_consistency`, `cleanup_after_alias_removed`,
//!   `store_persistence_cleaned_when_alias_removed`: check the saved
//!   credential, not the cooldown store's records.
//! - `real_persistence_restore_bypasses_cooling_auth` is dropped: there is
//!   no cooldown store to restore from. In its place,
//!   `credentials_reloaded_from_files_start_without_cooldowns` checks that a
//!   credential a new manager loads through `FileStore` has none.
//! - `cancelled_context_reconcile_cooldown_store_cleaned`,
//!   `strict_context_checking_store_reconcile_persists_under_cancelled_context`,
//!   `reset_quota_cancelled_context_cleans_cooldown_store`: there is no
//!   context to cancel, because cancellation is dropping the future. The
//!   calls are plain, and the checks use the saved credential.
//! - `reset_quota_error_persist_cleans_cooldown_store`: the failing store is
//!   the `FakeStore` set to fail saves. The cooldown store's record before
//!   the reset is the manager's own state.
//! - `mark_result_transient_errors_sync_suspend_registry`: `GetModelCount`
//!   counts the clients that serve the model and whose projection isn't
//!   suspended.
//! - `concurrent_reconcile_and_mark_result`,
//!   `concurrent_reconcile_mark_result_full_consistency`,
//!   `concurrent_operations_natural_final_consistency`: goroutines become
//!   futures joined on one thread with paused time. They interleave at each
//!   sleep, deterministically. The saved credential stands in for the store.
//! - `concurrent_high_load_stability_epoch_and_reconcile`: the same, with
//!   yields. The pick worker calls `execute`, which also records a success.
//! - `scheduler_stale_disabled_snapshot_cannot_remove_active_entry`,
//!   `scheduler_tombstone_stale_active_snapshot_cannot_reactivate_disabled_entry`,
//!   `scheduler_tombstone_remove_rejects_delayed_active_snapshot_and_rebuild`:
//!   the scheduler isn't a separate index that is fed snapshots, so stale
//!   snapshots and rebuilds can't be sent to it. The tests check picks
//!   around the real change instead: an update to disabled, or a remove.
//! - `remove_and_re_register_clears_scheduler_tombstone`: a credential can't
//!   be registered with a generation (upstream uses 5, then 1). The
//!   generation lives in the manager.
//! - `monotonic_registration_epoch_rejects_old_epoch_high_generation`: the
//!   registry half is left out, since the registry isn't part of the
//!   manager. The delayed old-epoch snapshot can't be sent.
//! - `update_rejects_stale_registration_epoch`: an update can't carry an
//!   epoch, because it always takes the live one. So the rejected stale
//!   update is left out; the valid update is checked.
//! - `remove_increments_auth_epoch_and_rejects_delayed_old_epoch_snapshot`,
//!   `scheduler_delayed_old_epoch_remove_tombstone_rejected_and_new_auth_schedulable`:
//!   epochs are read from the manager's state, not from `Auth`. The delayed
//!   snapshot or tombstone can't be sent.
//! - `reconcile_exhausted_retry_on_epoch_mismatch_preserves_state_and_no_stale_snapshot`:
//!   upstream races a goroutine against the reconcile. Here every attempt
//!   sees a newer registration epoch, through a registry that re-registers
//!   the client each time the manager checks. The test also checks that
//!   nothing was committed or published.
//! - `load_monotonic_epoch_assignment_and_removal_tombstones`: `Auth` has no
//!   epoch, so stored credentials can't carry one. The epochs are 1 and 1,
//!   then 2 and 2. Upstream's are 6 and 4, then 7 and 5: the same rule,
//!   counted from stored epochs 5 and 3. The stale snapshot can't be sent.
//! - Dropped `RealDiskFileStoreRestore_BypassesCoolingAuth`, for the same
//!   reason.
//! - Dropped `TestModelRegistry_Projection_GenerationProtection`,
//!   `TestModelRegistry_UnregisterClient_TombstoneAndGhostProjectionProtection`,
//!   `TestModelRegistry_UnregisterAndReRegister_ResetsTombstone` and
//!   `TestModelRegistry_ApplyClientModelProjections_StrictEpochAndBatchValidation`:
//!   they test the model registry, which isn't part of the manager. Here the
//!   registry is a fake.

use super::support::*;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::TimeDelta;
use serde_json::json;

use crate::auth::{
    Auth, AuthError, AuthStore, FileStore, ModelState, QuotaState, Status, Timestamp,
};
use crate::exec::Dispatcher;
use crate::manager::cooldown::{is_model_state_active_cooldown, model_state_is_clean};
use crate::manager::{CallResult, ClientModels, Manager, ModelAlias, ModelProjection, Settings};

const ANTIGRAVITY: &str = "antigravity";
const GEMINI: &str = "gemini";
const ROUTE: &str = "[ant]gemini-3.7-flash-high";
const TARGET: &str = "gemini-3.7-flash-high";

const MIN10: Duration = Duration::from_secs(10 * 60);
const MIN20: Duration = Duration::from_secs(20 * 60);
const MIN30: Duration = Duration::from_secs(30 * 60);

fn alias(name: &str, alias: &str) -> ModelAlias {
    ModelAlias {
        name: name.to_owned(),
        alias: alias.to_owned(),
        force_mapping: false,
    }
}

fn alias_settings(provider: &str, aliases: Vec<ModelAlias>) -> Settings {
    Settings {
        oauth_model_alias: BTreeMap::from([(provider.to_owned(), aliases)]),
        ..Settings::default()
    }
}

/// The usual no-fork alias: `ROUTE` stands for `TARGET`.
fn route_settings() -> Settings {
    alias_settings(ANTIGRAVITY, vec![alias(TARGET, ROUTE)])
}

fn active(id: &str, provider: &str) -> Auth {
    Auth {
        status: Status::Active,
        ..auth(id, provider)
    }
}

/// An active credential with metadata, which the manager saves.
fn saved_active(id: &str, provider: &str) -> Auth {
    Auth {
        status: Status::Active,
        ..auth_with_metadata(id, provider, json!({"type": "oauth"}))
    }
}

fn cooling(retry_after: Timestamp) -> ModelState {
    ModelState {
        unavailable: true,
        status: Status::Error,
        next_retry_after: Some(retry_after),
        ..ModelState::default()
    }
}

fn rate_limited(
    id: &str,
    provider: &str,
    model: &str,
    route_model: &str,
    message: &str,
    retry_after: Duration,
) -> CallResult {
    CallResult {
        auth_id: id.to_owned(),
        provider: provider.to_owned(),
        model: model.to_owned(),
        route_model: route_model.to_owned(),
        success: false,
        error: Some(AuthError {
            code: "rate_limit_exceeded".into(),
            message: message.into(),
            http_status: 429,
            ..AuthError::default()
        }),
        retry_after: Some(retry_after),
        ..CallResult::default()
    }
}

fn succeeded(id: &str, provider: &str, model: &str, route_model: &str) -> CallResult {
    CallResult {
        auth_id: id.to_owned(),
        provider: provider.to_owned(),
        model: model.to_owned(),
        route_model: route_model.to_owned(),
        success: true,
        ..CallResult::default()
    }
}

/// An executor answering with the model it was asked for, as upstream's
/// `noForkAliasTestExecutor`.
fn echo(provider: &str) -> Arc<FakeExecutor> {
    FakeExecutor::with(provider, |call: &Call| Reply::ok(call.model.clone()))
}

/// Upstream's `IsModelSuspendedForClient`.
fn suspended(h: &Harness, id: &str, model: &str) -> bool {
    h.models.projection(id, model).is_some_and(|p| p.suspended)
}

/// Upstream's `IsModelQuotaExceededForClient`.
fn quota_exceeded(h: &Harness, id: &str, model: &str) -> bool {
    h.models
        .projection(id, model)
        .is_some_and(|p| p.quota_exceeded)
}

/// Upstream's `GetModelCount` over `ids`: the clients serving `model` and
/// not suspended for it.
fn model_count(h: &Harness, ids: &[&str], model: &str) -> usize {
    ids.iter()
        .filter(|id| {
            h.models.models_for_client(id).iter().any(|m| m == model) && !suspended(h, id, model)
        })
        .count()
}

fn state(h: &Harness, id: &str, model: &str) -> Option<ModelState> {
    h.get(id).model_states.get(model).cloned()
}

fn stored_state(h: &Harness, id: &str, model: &str) -> Option<ModelState> {
    h.store
        .stored(id)
        .and_then(|auth| auth.model_states.get(model).cloned())
}

/// Whether the saved credential carries a cooldown for `model`: the stand-in
/// for upstream's cooldown store holding a record for it.
fn stored_cooldown(h: &Harness, id: &str, model: &str) -> bool {
    stored_state(h, id, model).is_some_and(|s| is_model_state_active_cooldown(&s, h.now()))
}

/// Upstream's "cooling" in the consistency tests.
fn is_cooling(state: &ModelState, now: Timestamp) -> bool {
    state.unavailable || state.next_retry_after.is_some_and(|t| t > now)
}

// Test 1
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_retains_cooldown_after_reconcile() {
    const ID: &str = "nofork-auth-1";
    let h = Harness::new(route_settings());
    h.executor(&echo(ANTIGRAVITY));

    let retry_after = h.now() + TimeDelta::minutes(30);
    let mut a = active(ID, ANTIGRAVITY);
    a.model_states.insert(
        TARGET.into(),
        ModelState {
            quota: QuotaState {
                exceeded: true,
                reason: "rate_limit_exceeded".into(),
                next_recover_at: Some(retry_after),
                ..QuotaState::default()
            },
            ..cooling(retry_after)
        },
    );
    h.add(a, &[]);
    h.models.register(ID, &[ROUTE]);

    h.manager.reconcile_registry_model_states(ID);

    let state = state(&h, ID, TARGET).expect("targetModel state was deleted after reconcile");
    assert!(state.unavailable);
    assert_eq!(state.next_retry_after, Some(retry_after));
    assert!(state.quota.exceeded);
    assert!(suspended(&h, ID, ROUTE), "routeModel was not suspended");
}

// Test 2
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_bypasses_cooling_auth_on_subsequent_request() {
    const ID1: &str = "nofork-cooling-auth";
    const ID2: &str = "nofork-available-auth";
    let h = Harness::new(route_settings());
    let exec = echo(ANTIGRAVITY);
    h.executor(&exec);

    let mut a1 = active(ID1, ANTIGRAVITY);
    a1.model_states
        .insert(TARGET.into(), cooling(h.now() + TimeDelta::minutes(30)));
    h.add(a1, &[ROUTE]);
    h.add(active(ID2, ANTIGRAVITY), &[ROUTE]);
    h.manager.reconcile_registry_model_states(ID1);
    h.manager.reconcile_registry_model_states(ID2);

    let resp = h
        .manager
        .execute(&providers(&[ANTIGRAVITY]), request(ROUTE), options())
        .await
        .expect("execute");
    assert_eq!(&resp.payload[..], TARGET.as_bytes());
    assert_eq!(
        exec.ids(Kind::Execute).last().map(String::as_str),
        Some(ID2),
        "auth1 should be skipped due to cooldown"
    );
    assert_eq!(exec.models(Kind::Execute), [TARGET]);
}

// Test 3
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_cooldown_persistence_consistency() {
    const ID: &str = "nofork-persist-auth";
    let h = Harness::with_store(route_settings());
    h.executor(&echo(ANTIGRAVITY));
    h.add(saved_active(ID, ANTIGRAVITY), &[ROUTE]);

    h.manager.mark_result(&rate_limited(
        ID,
        ANTIGRAVITY,
        TARGET,
        "",
        "429 rate limit",
        MIN30,
    ));
    assert!(h.store.save_count() > 0, "expected the credential saved");
    assert!(
        stored_cooldown(&h, ID, TARGET),
        "expected a saved cooldown for targetModel"
    );

    h.models.register(ID, &[ROUTE]);
    h.manager.reconcile_registry_model_states(ID);
    assert!(
        stored_cooldown(&h, ID, TARGET),
        "saved cooldown for targetModel was lost after reconcile"
    );
}

// Test 4
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_multiple_aliases_to_same_target() {
    const ID: &str = "nofork-multi-alias-auth";
    const ROUTE1: &str = "[ant1]gemini-3.7-flash-high";
    const ROUTE2: &str = "[ant2]gemini-3.7-flash-high";
    let h = Harness::new(alias_settings(
        ANTIGRAVITY,
        vec![alias(TARGET, ROUTE1), alias(TARGET, ROUTE2)],
    ));
    h.executor(&echo(ANTIGRAVITY));
    h.add(active(ID, ANTIGRAVITY), &[ROUTE1, ROUTE2]);

    h.manager.mark_result(&rate_limited(
        ID,
        ANTIGRAVITY,
        TARGET,
        "",
        "429 rate limit",
        MIN30,
    ));

    let current = h.get(ID);
    assert_eq!(current.model_states.len(), 1, "{:?}", current.model_states);
    assert!(current.model_states.contains_key(TARGET));
    assert!(suspended(&h, ID, ROUTE1));
    assert!(suspended(&h, ID, ROUTE2));

    h.models.register(ID, &[ROUTE1, ROUTE2]);
    h.manager.reconcile_registry_model_states(ID);
    assert!(suspended(&h, ID, ROUTE1), "routeModel1 lost suspension");
    assert!(suspended(&h, ID, ROUTE2), "routeModel2 lost suspension");
}

// Test 5
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_cleanup_after_alias_removed() {
    const ID: &str = "nofork-cleanup-auth";
    const OTHER: &str = "other-model";
    let h = Harness::with_store(route_settings());
    h.executor(&echo(ANTIGRAVITY));

    let mut a = saved_active(ID, ANTIGRAVITY);
    a.model_states
        .insert(TARGET.into(), cooling(h.now() + TimeDelta::minutes(30)));
    h.add(a, &[ROUTE]);

    h.manager.reconcile_registry_model_states(ID);
    assert!(
        state(&h, ID, TARGET).is_some(),
        "targetModel should exist before alias removal"
    );

    h.manager.set_settings(alias_settings(ANTIGRAVITY, vec![]));
    h.models.register(ID, &[OTHER]);
    h.manager.reconcile_registry_model_states(ID);

    assert!(
        state(&h, ID, TARGET).is_none(),
        "targetModel should have been deleted after alias was removed"
    );
    assert!(
        stored_state(&h, ID, TARGET).is_none(),
        "saved state for deleted targetModel still exists"
    );
}

// Test 6
#[tokio::test(start_paused = true)]
async fn manager_fork_true_retains_state_and_suspends_both() {
    const ID: &str = "fork-true-auth";
    const ROUTE_F: &str = "claude-opus-4-6";
    const TARGET_F: &str = "claude-opus-4-6-thinking";
    let h = Harness::new(alias_settings(ANTIGRAVITY, vec![alias(TARGET_F, ROUTE_F)]));
    h.executor(&echo(ANTIGRAVITY));

    let mut a = active(ID, ANTIGRAVITY);
    a.model_states
        .insert(TARGET_F.into(), cooling(h.now() + TimeDelta::minutes(30)));
    h.add(a, &[ROUTE_F, TARGET_F]);

    h.manager.reconcile_registry_model_states(ID);

    let state = state(&h, ID, TARGET_F).expect("targetModel state was deleted");
    assert!(state.unavailable);
    assert!(suspended(&h, ID, TARGET_F));
    assert!(suspended(&h, ID, ROUTE_F));
}

// Test 7
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_expired_cooldown_reset_clean() {
    const ID: &str = "nofork-expired-auth";
    let h = Harness::new(route_settings());
    h.executor(&echo(ANTIGRAVITY));

    let mut a = active(ID, ANTIGRAVITY);
    a.model_states.insert(
        TARGET.into(),
        ModelState {
            status_message: "old error".into(),
            ..cooling(h.now() - TimeDelta::minutes(10))
        },
    );
    h.add(a, &[ROUTE]);

    h.manager.reconcile_registry_model_states(ID);

    if let Some(state) = state(&h, ID, TARGET) {
        assert!(
            model_state_is_clean(&state),
            "expired state not cleaned: {state:?}"
        );
    }
}

// Test 8
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_mark_result_and_reset_quota_projection() {
    const ID: &str = "nofork-mark-reset-auth";
    let h = Harness::new(route_settings());
    h.executor(&echo(ANTIGRAVITY));
    h.add(active(ID, ANTIGRAVITY), &[ROUTE]);

    h.manager.mark_result(&rate_limited(
        ID,
        ANTIGRAVITY,
        TARGET,
        "",
        "429 rate limit",
        MIN30,
    ));
    assert!(quota_exceeded(&h, ID, ROUTE));
    assert!(suspended(&h, ID, ROUTE));

    h.manager.reset_quota(ID).expect("ResetQuota failed");
    assert!(!quota_exceeded(&h, ID, ROUTE));
    assert!(!suspended(&h, ID, ROUTE));
}

// Test 9
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_per_auth_alias_retains_cooldown_after_reconcile() {
    const ID: &str = "nofork-perauth-auth-1";
    const GLOBAL: &str = "gemini-3.7-flash-global";
    const PER_AUTH: &str = "gemini-3.7-flash-perauth";
    let h = Harness::new(alias_settings(ANTIGRAVITY, vec![alias(GLOBAL, ROUTE)]));
    h.executor(&echo(ANTIGRAVITY));

    let retry_after = h.now() + TimeDelta::minutes(30);
    let mut a = active(ID, ANTIGRAVITY);
    a.model_states.insert(
        PER_AUTH.into(),
        ModelState {
            quota: QuotaState {
                exceeded: true,
                reason: "rate_limit_exceeded".into(),
                next_recover_at: Some(retry_after),
                ..QuotaState::default()
            },
            ..cooling(retry_after)
        },
    );
    a.model_states.insert(GLOBAL.into(), cooling(retry_after));
    // Upstream's SetOAuthModelAliasesAttribute.
    a.attributes.insert(
        "model_aliases".into(),
        json!([{"name": PER_AUTH, "alias": ROUTE, "fork": false}]).to_string(),
    );
    h.add(a, &[ROUTE]);

    h.manager.reconcile_registry_model_states(ID);

    let state = state(&h, ID, PER_AUTH).expect("perAuthTarget state was deleted");
    assert!(
        state.unavailable && state.quota.exceeded,
        "active cooldown lost: {state:?}"
    );
    assert!(
        !h.get(ID).model_states.contains_key(GLOBAL),
        "overridden globalTarget should have been pruned"
    );
    assert!(suspended(&h, ID, ROUTE));
    assert!(quota_exceeded(&h, ID, ROUTE));
}

// In place of test 10
#[tokio::test(start_paused = true)]
async fn credentials_reloaded_from_files_start_without_cooldowns() {
    const ID: &str = "restore-auth-1.json";
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = TestClock::new();
    let manager = || {
        let store: Arc<dyn AuthStore> = Arc::new(FileStore::new(dir.path()));
        Manager::with_clock(
            route_settings(),
            Arc::new(FakeModels::default()),
            Some(store),
            clock.clock(),
        )
    };

    let first = manager();
    let mut credential = saved_active(ID, ANTIGRAVITY);
    credential.file_name = ID.to_owned();
    first.register(credential).expect("register");
    first.mark_result(&rate_limited(
        ID,
        ANTIGRAVITY,
        TARGET,
        "",
        "429 rate limit",
        MIN30,
    ));
    let cooled = first.get(ID).expect("registered");
    assert!(
        cooled
            .model_states
            .get(TARGET)
            .is_some_and(|s| is_model_state_active_cooldown(s, clock.now())),
        "expected a cooldown: {:?}",
        cooled.model_states
    );

    let second = manager();
    second.load().expect("load");
    let restored = second.get(ID).expect("restored credential");
    assert!(
        restored.model_states.is_empty(),
        "restored model states: {:?}",
        restored.model_states
    );
    assert!(!restored.unavailable);
}

// Test 11
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_store_persistence_cleaned_when_alias_removed() {
    const ID: &str = "nofork-store-clean-auth";
    const OTHER: &str = "other-clean-model";
    let h = Harness::with_store(route_settings());
    h.executor(&echo(ANTIGRAVITY));
    h.add(saved_active(ID, ANTIGRAVITY), &[ROUTE]);

    h.manager.mark_result(&rate_limited(
        ID,
        ANTIGRAVITY,
        TARGET,
        "",
        "429 rate limit",
        MIN30,
    ));
    assert!(
        stored_cooldown(&h, ID, TARGET),
        "expected a saved cooldown before alias removal"
    );

    h.manager.set_settings(alias_settings(ANTIGRAVITY, vec![]));
    h.models.register(ID, &[OTHER]);
    h.manager.reconcile_registry_model_states(ID);

    assert!(
        stored_state(&h, ID, TARGET).is_none(),
        "saved state for targetModel should have been removed"
    );
}

// Test 12
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_multiple_aliases_both_skip_cooling_auth_end_to_end() {
    const ID1: &str = "multi-cooling-auth";
    const ID2: &str = "multi-available-auth";
    const ROUTE1: &str = "[ant1]gemini-3.7-flash-high";
    const ROUTE2: &str = "[ant2]gemini-3.7-flash-high";
    let h = Harness::new(alias_settings(
        ANTIGRAVITY,
        vec![alias(TARGET, ROUTE1), alias(TARGET, ROUTE2)],
    ));
    let exec = echo(ANTIGRAVITY);
    h.executor(&exec);

    let mut a1 = active(ID1, ANTIGRAVITY);
    a1.model_states
        .insert(TARGET.into(), cooling(h.now() + TimeDelta::minutes(30)));
    h.add(a1, &[ROUTE1, ROUTE2]);
    h.add(active(ID2, ANTIGRAVITY), &[ROUTE1, ROUTE2]);
    h.manager.reconcile_registry_model_states(ID1);
    h.manager.reconcile_registry_model_states(ID2);

    for route in [ROUTE1, ROUTE2] {
        let resp = h
            .manager
            .execute(&providers(&[ANTIGRAVITY]), request(route), options())
            .await
            .expect("execute");
        assert_eq!(&resp.payload[..], TARGET.as_bytes(), "{route}");
        assert_eq!(
            exec.ids(Kind::Execute).last().map(String::as_str),
            Some(ID2),
            "{route}"
        );
    }
}

// Test 13
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_alias_repointing_prunes_old_target_and_reconciles() {
    const MODEL_A: &str = "gemini-model-a";
    const MODEL_B: &str = "[alias]gemini-model-b";
    const MODEL_C: &str = "gemini-model-c";

    // Repointing B from A to C.
    {
        const ID: &str = "repoint-auth-1";
        let h = Harness::new(alias_settings(ANTIGRAVITY, vec![alias(MODEL_A, MODEL_B)]));
        h.executor(&echo(ANTIGRAVITY));
        let mut a = active(ID, ANTIGRAVITY);
        a.model_states
            .insert(MODEL_A.into(), cooling(h.now() + TimeDelta::minutes(30)));
        h.add(a, &[MODEL_B]);

        h.manager.reconcile_registry_model_states(ID);
        assert!(suspended(&h, ID, MODEL_B), "modelB should be suspended");

        h.manager
            .set_settings(alias_settings(ANTIGRAVITY, vec![alias(MODEL_C, MODEL_B)]));
        h.manager.reconcile_registry_model_states(ID);

        assert!(
            state(&h, ID, MODEL_A).is_none(),
            "old modelA state should have been pruned"
        );
        assert!(
            !suspended(&h, ID, MODEL_B),
            "modelB should no longer be suspended"
        );

        h.manager.mark_result(&rate_limited(
            ID,
            ANTIGRAVITY,
            MODEL_C,
            "",
            "429 rate limit",
            MIN30,
        ));
        assert!(
            suspended(&h, ID, MODEL_B),
            "modelB should now be suspended after modelC cooldown"
        );
    }

    // Route named A mapped to C.
    {
        const ID: &str = "repoint-auth-2";
        let h = Harness::new(alias_settings(ANTIGRAVITY, vec![alias(MODEL_C, MODEL_A)]));
        h.executor(&echo(ANTIGRAVITY));
        let mut a = active(ID, ANTIGRAVITY);
        a.model_states
            .insert(MODEL_A.into(), cooling(h.now() + TimeDelta::minutes(30)));
        h.add(a, &[MODEL_A]);

        h.manager.reconcile_registry_model_states(ID);

        assert!(
            state(&h, ID, MODEL_A).is_none(),
            "legacy modelA state should have been pruned"
        );
        assert!(
            state(&h, ID, MODEL_C).is_some_and(|s| s.unavailable),
            "modelC should have inherited the cooldown"
        );
        assert!(
            suspended(&h, ID, MODEL_A),
            "route modelA should be suspended"
        );
    }
}

// Test 14
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_concurrent_reconcile_and_mark_result() {
    const ID: &str = "nofork-concurrent-auth";
    const ITERATIONS: usize = 100;
    let h = Harness::with_store(route_settings());
    h.executor(&echo(ANTIGRAVITY));
    h.add(saved_active(ID, ANTIGRAVITY), &[ROUTE]);

    let tick = || tokio::time::sleep(Duration::from_millis(1));
    let reconciler = async {
        for _ in 0..ITERATIONS {
            h.manager.reconcile_registry_model_states(ID);
            tick().await;
        }
    };
    let marker = async {
        for i in 0..ITERATIONS {
            if i % 2 == 0 {
                h.manager.mark_result(&rate_limited(
                    ID,
                    ANTIGRAVITY,
                    TARGET,
                    "",
                    "429 rate limit",
                    MIN10,
                ));
            } else {
                let _ = h.manager.reset_quota(ID);
            }
            tick().await;
        }
    };
    let caller = async {
        for _ in 0..ITERATIONS {
            let _ = h
                .manager
                .execute(&providers(&[ANTIGRAVITY]), request(ROUTE), options())
                .await;
            tick().await;
        }
    };
    tokio::join!(reconciler, marker, caller);
}

// Test 15
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_mark_result_route_model_writes_authoritative_state() {
    const ID: &str = "auth-mark-route-b";
    let h = Harness::new(route_settings());
    h.executor(&echo(ANTIGRAVITY));
    h.add(active(ID, ANTIGRAVITY), &[ROUTE]);

    h.manager.mark_result(&rate_limited(
        ID,
        ANTIGRAVITY,
        "",
        ROUTE,
        "429 rate limit",
        MIN20,
    ));

    assert!(
        state(&h, ID, ROUTE).is_none(),
        "ModelStates[routeModel] was created"
    );
    let state_a = state(&h, ID, TARGET).expect("authoritative ModelStates[targetModel] missing");
    assert!(
        state_a.unavailable && state_a.quota.exceeded,
        "not cooling: {state_a:?}"
    );
    assert!(suspended(&h, ID, ROUTE));

    h.manager
        .mark_result(&succeeded(ID, ANTIGRAVITY, "", ROUTE));

    assert!(
        state(&h, ID, ROUTE).is_none(),
        "ModelStates[routeModel] was created after success"
    );
    let state_a = state(&h, ID, TARGET);
    assert!(
        state_a.as_ref().is_some_and(model_state_is_clean),
        "targetModel should be clean after success: {state_a:?}"
    );
    assert!(!suspended(&h, ID, ROUTE));
}

// Test 16
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_legacy_model_state_pruned_on_reconcile() {
    const ID: &str = "auth-legacy-prune";
    let h = Harness::new(route_settings());
    h.executor(&echo(ANTIGRAVITY));

    let retry_after = h.now() + TimeDelta::minutes(30);
    let mut a = active(ID, ANTIGRAVITY);
    a.model_states.insert(
        ROUTE.into(),
        ModelState {
            status_message: "legacy route state".into(),
            ..cooling(retry_after)
        },
    );
    a.model_states.insert(
        TARGET.into(),
        ModelState {
            status_message: "authoritative target state".into(),
            ..cooling(retry_after)
        },
    );
    h.add(a, &[ROUTE, TARGET]);

    h.manager.reconcile_registry_model_states(ID);

    assert!(
        state(&h, ID, ROUTE).is_none(),
        "legacy ModelStates[routeModel] was not pruned"
    );
    let state_a = state(&h, ID, TARGET).expect("authoritative ModelStates[targetModel] missing");
    assert!(state_a.unavailable);
    assert!(suspended(&h, ID, ROUTE));
    assert!(suspended(&h, ID, TARGET));
}

/// The final checks of upstream's natural-consistency tests: no route state,
/// the projection follows the target's cooldown, and the saved credential
/// agrees with memory.
fn assert_natural_consistency(h: &Harness, id: &str, route: &str, target: &str) {
    let now = h.now();
    let final_auth = h.get(id);
    assert!(
        !final_auth.model_states.contains_key(route),
        "ModelStates contains routeModel after concurrency"
    );
    let target_cooling = final_auth
        .model_states
        .get(target)
        .is_some_and(|s| is_cooling(s, now));
    assert_eq!(
        target_cooling,
        suspended(h, id, route),
        "registry suspended mismatch"
    );

    let stored = h.store.stored(id).expect("saved credential");
    assert!(
        !stored.model_states.contains_key(route),
        "store contains a state for routeModel"
    );
    let stored_cooling = stored
        .model_states
        .get(target)
        .is_some_and(|s| is_cooling(s, now));
    assert_eq!(
        stored_cooling, target_cooling,
        "saved cooldown doesn't match memory"
    );
}

// Test 18
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_concurrent_reconcile_mark_result_full_consistency() {
    const ID: &str = "concurrent-consistency-auth";
    const ITERATIONS: usize = 100;
    let h = Harness::with_store(route_settings());
    h.executor(&echo(ANTIGRAVITY));
    h.add(saved_active(ID, ANTIGRAVITY), &[ROUTE]);

    let tick = || tokio::time::sleep(Duration::from_micros(50));
    let reconciler = async {
        for _ in 0..ITERATIONS {
            h.manager.reconcile_registry_model_states(ID);
            tick().await;
        }
    };
    let route_marker = async {
        for i in 0..ITERATIONS {
            match i % 3 {
                0 => h.manager.mark_result(&rate_limited(
                    ID,
                    ANTIGRAVITY,
                    TARGET,
                    ROUTE,
                    "429 rate limit",
                    MIN10,
                )),
                1 => h
                    .manager
                    .mark_result(&succeeded(ID, ANTIGRAVITY, TARGET, ROUTE)),
                _ => {
                    let _ = h.manager.reset_quota(ID);
                }
            }
            tick().await;
        }
    };
    let target_marker = async {
        for i in 0..ITERATIONS {
            if i % 2 == 0 {
                h.manager.mark_result(&rate_limited(
                    ID,
                    ANTIGRAVITY,
                    TARGET,
                    "",
                    "429 rate limit",
                    MIN10,
                ));
            } else {
                let _ = h.manager.reset_quota(ID);
            }
            tick().await;
        }
    };
    let caller = async {
        for _ in 0..ITERATIONS {
            let _ = h
                .manager
                .execute(&providers(&[ANTIGRAVITY]), request(ROUTE), options())
                .await;
            tick().await;
        }
    };
    tokio::join!(reconciler, route_marker, target_marker, caller);

    assert_natural_consistency(&h, ID, ROUTE, TARGET);
}

/// Upstream's tests 19 and 23: a cooldown, then the alias goes and the
/// registry lists another model; reconciling prunes the state in memory and
/// in the saved credential.
fn alias_removed_reconcile_cleans(id: &str, route: &str, target: &str, other: &str) {
    let h = Harness::with_store(alias_settings(ANTIGRAVITY, vec![alias(target, route)]));
    h.executor(&echo(ANTIGRAVITY));
    h.add(saved_active(id, ANTIGRAVITY), &[route]);

    h.manager.mark_result(&rate_limited(
        id,
        ANTIGRAVITY,
        target,
        route,
        "429 rate limit",
        MIN30,
    ));
    assert!(
        stored_cooldown(&h, id, target),
        "expected a saved cooldown for targetModel initially"
    );

    h.manager.set_settings(alias_settings(ANTIGRAVITY, vec![]));
    h.models.register(id, &[other]);
    h.manager.reconcile_registry_model_states(id);

    assert!(
        state(&h, id, target).is_none(),
        "targetModel was not pruned from memory ModelStates"
    );
    assert!(
        stored_state(&h, id, target).is_none(),
        "targetModel still in the saved credential"
    );
}

// Test 19
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_cancelled_context_reconcile_cooldown_store_cleaned() {
    alias_removed_reconcile_cleans("nofork-ctx-cancel-auth", ROUTE, TARGET, "other-clean-model");
}

// Test 20
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_mark_result_target_model_no_secondary_alias_resolution() {
    const ID: &str = "auth-no-secondary-resolve";
    const MODEL_B: &str = "route-model-b";
    const MODEL_A: &str = "target-model-a";
    const MODEL_C: &str = "chained-model-c";
    let h = Harness::new(alias_settings(
        ANTIGRAVITY,
        vec![alias(MODEL_A, MODEL_B), alias(MODEL_C, MODEL_A)],
    ));
    h.executor(&echo(ANTIGRAVITY));
    h.add(active(ID, ANTIGRAVITY), &[MODEL_B]);

    h.manager.mark_result(&rate_limited(
        ID,
        ANTIGRAVITY,
        MODEL_A,
        MODEL_B,
        "429 rate limit",
        MIN20,
    ));

    let state_a = state(&h, ID, MODEL_A).expect("expected ModelStates[modelA]");
    assert!(
        state_a.unavailable && state_a.quota.exceeded,
        "not cooling: {state_a:?}"
    );
    assert!(
        state(&h, ID, MODEL_C).is_none(),
        "secondary alias resolution created ModelStates[modelC]"
    );
    assert!(state(&h, ID, MODEL_B).is_none());
    assert!(suspended(&h, ID, MODEL_B));
}

// Test 21
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_mark_result_transient_errors_sync_suspend_registry() {
    const ID: &str = "auth-transient-suspend";
    const ROUTE_T: &str = "[ant]gemini-3.7-flash-transient";
    const TARGET_T: &str = "gemini-3.7-flash-transient";
    let h = Harness::new(alias_settings(ANTIGRAVITY, vec![alias(TARGET_T, ROUTE_T)]));
    h.executor(&echo(ANTIGRAVITY));
    h.add(active(ID, ANTIGRAVITY), &[ROUTE_T]);

    assert!(!suspended(&h, ID, ROUTE_T));
    assert_eq!(model_count(&h, &[ID], ROUTE_T), 1);

    let failure = |code: &str, message: &str, status: u16| CallResult {
        auth_id: ID.into(),
        provider: ANTIGRAVITY.into(),
        model: TARGET_T.into(),
        route_model: ROUTE_T.into(),
        success: false,
        error: Some(AuthError {
            code: code.into(),
            message: message.into(),
            http_status: status,
            ..AuthError::default()
        }),
        ..CallResult::default()
    };

    h.manager
        .mark_result(&failure("internal_error", "500 Internal Server Error", 500));
    assert!(suspended(&h, ID, ROUTE_T), "should be suspended after 500");
    assert_eq!(model_count(&h, &[ID], ROUTE_T), 0);

    h.manager.mark_result(&failure(
        "service_unavailable",
        "503 Service Unavailable",
        503,
    ));
    assert!(
        suspended(&h, ID, ROUTE_T),
        "should remain suspended after 503"
    );

    h.manager
        .mark_result(&succeeded(ID, ANTIGRAVITY, TARGET_T, ROUTE_T));
    assert!(
        !suspended(&h, ID, ROUTE_T),
        "should be resumed after success"
    );
    assert_eq!(model_count(&h, &[ID], ROUTE_T), 1);
}

// Test 22
#[tokio::test(start_paused = true)]
async fn manager_scheduler_stale_disabled_snapshot_cannot_remove_active_entry() {
    const ID: &str = "auth-gen-race-test";
    const MODEL: &str = "gemini-3.7-flash-gen-test";
    let h = Harness::new(Settings::default());
    let exec = echo(ANTIGRAVITY);
    h.executor(&exec);
    h.add(active(ID, ANTIGRAVITY), &[MODEL]);

    let (_, registered_gen) = h.versions(ID);
    assert!(registered_gen >= 1, "initial generation {registered_gen}");

    h.manager
        .mark_result(&succeeded(ID, ANTIGRAVITY, MODEL, ""));
    let (_, active_gen) = h.versions(ID);
    assert!(
        active_gen > registered_gen,
        "{active_gen} <= {registered_gen}"
    );

    assert_eq!(
        pick_by_call(&h, &exec, ANTIGRAVITY, MODEL)
            .await
            .expect("pick"),
        ID
    );
    // A stale disabled snapshot can't reach the scheduler here; the active
    // credential stays pickable.
    assert_eq!(
        pick_by_call(&h, &exec, ANTIGRAVITY, MODEL)
            .await
            .expect("pick after stale"),
        ID
    );

    let current = h.get(ID);
    h.manager
        .update(Auth {
            disabled: true,
            ..(*current).clone()
        })
        .expect("update")
        .expect("updated auth");
    assert!(
        pick_by_call(&h, &exec, ANTIGRAVITY, MODEL).await.is_err(),
        "auth still picked after the disabled update"
    );
}

// Test 23
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_strict_context_checking_store_reconcile_persists_under_cancelled_context()
 {
    alias_removed_reconcile_cleans(
        "strict-ctx-auth",
        "[ant]gemini-3.7-flash-strict",
        "gemini-3.7-flash-strict",
        "other-model-strict",
    );
}

// Test 24
#[tokio::test(start_paused = true)]
async fn manager_scheduler_tombstone_stale_active_snapshot_cannot_reactivate_disabled_entry() {
    const ID: &str = "auth-tombstone-test";
    const MODEL: &str = "gemini-3.7-tombstone-test";
    let h = Harness::new(Settings::default());
    let exec = echo(ANTIGRAVITY);
    h.executor(&exec);
    let registered = h.add(active(ID, ANTIGRAVITY), &[MODEL]);

    assert_eq!(
        pick_by_call(&h, &exec, ANTIGRAVITY, MODEL)
            .await
            .expect("initial pick"),
        ID
    );

    h.manager
        .update(Auth {
            disabled: true,
            status: Status::Disabled,
            ..(*registered).clone()
        })
        .expect("update")
        .expect("updated auth");
    assert!(
        pick_by_call(&h, &exec, ANTIGRAVITY, MODEL).await.is_err(),
        "pick succeeded after the disabled update"
    );
    // The delayed active snapshot can't reach the scheduler here; the
    // credential stays disabled.
    assert!(
        pick_by_call(&h, &exec, ANTIGRAVITY, MODEL).await.is_err(),
        "pick succeeded after the stale active snapshot"
    );
}

// Test 26
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_historical_route_state_only_migrated_to_target_cooldown() {
    const ID: &str = "auth-historical-mig-test";
    const ROUTE_B: &str = "[ant]gemini-3.7-flash-mig";
    const TARGET_A: &str = "gemini-3.7-flash-mig";
    let h = Harness::new(alias_settings(ANTIGRAVITY, vec![alias(TARGET_A, ROUTE_B)]));
    h.executor(&echo(ANTIGRAVITY));

    let mut a = active(ID, ANTIGRAVITY);
    a.model_states.insert(
        ROUTE_B.into(),
        ModelState {
            status_message: "historical cooling state on route B".into(),
            ..cooling(h.now() + TimeDelta::minutes(30))
        },
    );
    h.add(a, &[ROUTE_B]);

    h.manager.reconcile_registry_model_states(ID);

    assert!(
        state(&h, ID, ROUTE_B).is_none(),
        "historical ModelStates[routeModelB] was not pruned"
    );
    let state_a = state(&h, ID, TARGET_A).expect("authoritative ModelStates[targetModelA] missing");
    assert!(
        state_a.unavailable && state_a.next_retry_after.is_some(),
        "lost cooldown during migration: {state_a:?}"
    );
    assert!(suspended(&h, ID, ROUTE_B));
}

// Test 27
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_reset_quota_cancelled_context_cleans_cooldown_store() {
    const ID: &str = "auth-resetquota-ctx-test";
    const ROUTE_R: &str = "[ant]gemini-3.7-flash-rq-test";
    const TARGET_R: &str = "gemini-3.7-flash-rq-test";
    let h = Harness::with_store(alias_settings(ANTIGRAVITY, vec![alias(TARGET_R, ROUTE_R)]));
    h.executor(&echo(ANTIGRAVITY));
    h.add(saved_active(ID, ANTIGRAVITY), &[ROUTE_R]);

    h.manager.mark_result(&rate_limited(
        ID,
        ANTIGRAVITY,
        TARGET_R,
        ROUTE_R,
        "429 rate limit",
        MIN30,
    ));
    assert!(
        stored_cooldown(&h, ID, TARGET_R),
        "expected a saved cooldown before ResetQuota"
    );

    let _ = h.manager.reset_quota(ID);

    assert!(
        !stored_cooldown(&h, ID, TARGET_R),
        "saved cooldown was not cleared by ResetQuota"
    );
    assert!(!suspended(&h, ID, ROUTE_R));
}

// Test 28
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_concurrent_operations_natural_final_consistency() {
    const ID: &str = "auth-nat-consistency";
    const ROUTE_N: &str = "[ant]gemini-3.7-flash-nat-test";
    const TARGET_N: &str = "gemini-3.7-flash-nat-test";
    const ITERATIONS: usize = 80;
    let h = Harness::with_store(alias_settings(ANTIGRAVITY, vec![alias(TARGET_N, ROUTE_N)]));
    h.executor(&echo(ANTIGRAVITY));
    h.add(saved_active(ID, ANTIGRAVITY), &[ROUTE_N]);

    let tick = || tokio::time::sleep(Duration::from_micros(100));
    let reconciler = async {
        for _ in 0..ITERATIONS {
            h.manager.reconcile_registry_model_states(ID);
            tick().await;
        }
    };
    let marker = async {
        for i in 0..ITERATIONS {
            if i % 2 == 0 {
                h.manager.mark_result(&rate_limited(
                    ID,
                    ANTIGRAVITY,
                    TARGET_N,
                    ROUTE_N,
                    "429",
                    MIN10,
                ));
            } else {
                h.manager
                    .mark_result(&succeeded(ID, ANTIGRAVITY, TARGET_N, ROUTE_N));
            }
            tick().await;
        }
    };
    let resetter = async {
        for i in 0..ITERATIONS {
            if i % 3 == 0 {
                let _ = h.manager.reset_quota(ID);
            }
            tick().await;
        }
    };
    let caller = async {
        for _ in 0..ITERATIONS {
            let _ = h
                .manager
                .execute(&providers(&[ANTIGRAVITY]), request(ROUTE_N), options())
                .await;
            tick().await;
        }
    };
    tokio::join!(reconciler, marker, resetter, caller);

    assert_natural_consistency(&h, ID, ROUTE_N, TARGET_N);
}

// Test 29
#[tokio::test(start_paused = true)]
async fn manager_scheduler_tombstone_remove_rejects_delayed_active_snapshot_and_rebuild() {
    const ID: &str = "auth-remove-tombstone-test";
    const MODEL: &str = "gemini-3.7-flash-remove-tombstone";
    let h = Harness::new(Settings::default());
    let exec = echo(ANTIGRAVITY);
    h.executor(&exec);
    h.add(active(ID, ANTIGRAVITY), &[MODEL]);

    assert_eq!(
        pick_by_call(&h, &exec, ANTIGRAVITY, MODEL)
            .await
            .expect("initial pick"),
        ID
    );

    h.manager.remove(ID);
    assert!(
        pick_by_call(&h, &exec, ANTIGRAVITY, MODEL).await.is_err(),
        "expected auth_not_found after Remove"
    );
    // Delayed snapshots and rebuilds can't reach the scheduler here; the
    // removed credential stays unpickable.
    assert!(
        pick_by_call(&h, &exec, ANTIGRAVITY, MODEL).await.is_err(),
        "removed auth was picked again"
    );
}

// Test 31
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_chained_alias_mark_result_target_a_reconcile_preserves_state_a() {
    const ID1: &str = "auth1-chain-test";
    const ID2: &str = "auth2-chain-test";
    const ROUTE_B: &str = "route-model-b-chain";
    const TARGET_A: &str = "canonical-model-a-chain";
    const TARGET_C: &str = "target-model-c-chain";
    let h = Harness::new(alias_settings(
        ANTIGRAVITY,
        vec![alias(TARGET_A, ROUTE_B), alias(TARGET_C, TARGET_A)],
    ));
    let exec = echo(ANTIGRAVITY);
    h.executor(&exec);
    h.add(active(ID1, ANTIGRAVITY), &[ROUTE_B]);
    h.add(active(ID2, ANTIGRAVITY), &[ROUTE_B]);

    h.manager.mark_result(&rate_limited(
        ID1,
        ANTIGRAVITY,
        TARGET_A,
        ROUTE_B,
        "429",
        MIN30,
    ));
    h.manager.reconcile_registry_model_states(ID1);

    let state_a = state(&h, ID1, TARGET_A).expect("authoritative ModelStates[targetA] missing");
    assert!(
        state_a.unavailable && state_a.next_retry_after.is_some(),
        "lost cooldown: {state_a:?}"
    );
    assert!(
        state(&h, ID1, TARGET_C).is_none(),
        "ModelStates contains targetC"
    );
    assert!(
        state(&h, ID1, ROUTE_B).is_none(),
        "ModelStates contains routeB"
    );
    assert!(suspended(&h, ID1, ROUTE_B));
    assert!(!suspended(&h, ID2, ROUTE_B));

    let resp = h
        .manager
        .execute(&providers(&[ANTIGRAVITY]), request(ROUTE_B), options())
        .await
        .expect("execute");
    assert_eq!(&resp.payload[..], TARGET_A.as_bytes());
    assert_eq!(
        exec.ids(Kind::Execute).last().map(String::as_str),
        Some(ID2),
        "Auth1 was not skipped"
    );
}

// Test 33
#[tokio::test(start_paused = true)]
async fn manager_remove_and_re_register_clears_scheduler_tombstone() {
    const ID: &str = "auth-rereg-tombstone-test";
    const MODEL: &str = "gemini-3.7-flash-rereg-test";
    let h = Harness::new(Settings::default());
    let exec = echo(GEMINI);
    h.executor(&exec);
    h.add(active(ID, GEMINI), &[MODEL]);

    assert_eq!(
        pick_by_call(&h, &exec, GEMINI, MODEL)
            .await
            .expect("initial pick"),
        ID
    );

    h.manager.remove(ID);
    assert!(
        pick_by_call(&h, &exec, GEMINI, MODEL).await.is_err(),
        "pick succeeded after Remove"
    );

    h.add(active(ID, GEMINI), &[]);
    assert_eq!(
        pick_by_call(&h, &exec, GEMINI, MODEL)
            .await
            .expect("pick after re-register"),
        ID
    );
}

// Test 34
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_chained_alias_both_routes_registered_safe_two_phase_migration() {
    const ID: &str = "auth-chain-both-test";
    const ROUTE_B: &str = "route-model-b-chain-both";
    const TARGET_A: &str = "canonical-model-a-chain-both";
    const TARGET_C: &str = "target-model-c-chain-both";
    let h = Harness::new(alias_settings(
        ANTIGRAVITY,
        vec![alias(TARGET_A, ROUTE_B), alias(TARGET_C, TARGET_A)],
    ));
    h.executor(&echo(ANTIGRAVITY));
    h.add(active(ID, ANTIGRAVITY), &[ROUTE_B, TARGET_A]);

    h.manager.mark_result(&rate_limited(
        ID,
        ANTIGRAVITY,
        TARGET_A,
        ROUTE_B,
        "429",
        MIN30,
    ));
    h.manager.reconcile_registry_model_states(ID);

    let state_a = state(&h, ID, TARGET_A).expect("authoritative ModelStates[targetA] missing");
    assert!(
        state_a.unavailable && state_a.next_retry_after.is_some(),
        "lost cooldown: {state_a:?}"
    );
    assert!(
        state(&h, ID, TARGET_C).is_none(),
        "ModelStates contains targetC"
    );
    assert!(
        state(&h, ID, ROUTE_B).is_none(),
        "ModelStates contains routeB"
    );
}

// Test 35
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_reset_quota_error_persist_cleans_cooldown_store() {
    const ID: &str = "auth-err-persist-test";
    const ROUTE_E: &str = "route-model-err-persist";
    const TARGET_E: &str = "target-model-err-persist";
    let h = Harness::with_store(alias_settings(ANTIGRAVITY, vec![alias(TARGET_E, ROUTE_E)]));
    h.store.set_fail_saves(true);
    h.executor(&echo(ANTIGRAVITY));
    h.add(saved_active(ID, ANTIGRAVITY), &[ROUTE_E]);

    h.manager.mark_result(&rate_limited(
        ID,
        ANTIGRAVITY,
        TARGET_E,
        ROUTE_E,
        "429",
        MIN30,
    ));
    // The cooldown store's records stand for the cooldown the manager holds.
    assert!(
        state(&h, ID, TARGET_E).is_some_and(|s| is_model_state_active_cooldown(&s, h.now())),
        "expected a cooldown before ResetQuota"
    );

    assert!(
        h.manager.reset_quota(ID).is_err(),
        "expected ResetQuota to return the store's error"
    );

    assert!(
        state(&h, ID, TARGET_E).is_none_or(|s| model_state_is_clean(&s)),
        "cooldown not cleared after ResetQuota"
    );
    assert!(!suspended(&h, ID, ROUTE_E));
}

// Test 36
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_per_model_isolation_no_aggregation_fallback_flip() {
    const ID: &str = "auth-isolation-test";
    const MODEL_A: &str = "model-a-isolated";
    const MODEL_C: &str = "model-c-isolated";
    let h = Harness::new(Settings::default());
    h.executor(&echo(GEMINI));
    h.add(active(ID, GEMINI), &[MODEL_A, MODEL_C]);

    h.manager
        .mark_result(&rate_limited(ID, GEMINI, MODEL_A, MODEL_A, "429", MIN30));
    h.manager.reconcile_registry_model_states(ID);

    assert!(suspended(&h, ID, MODEL_A), "model A should be suspended");
    assert!(
        !suspended(&h, ID, MODEL_C),
        "model C was suspended by the aggregated availability"
    );
}

// Test 37
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_monotonic_registration_epoch_rejects_old_epoch_high_generation() {
    const ID: &str = "auth-epoch-gen-test";
    const MODEL: &str = "gemini-epoch-test-model";
    let h = Harness::new(Settings::default());
    let exec = echo(GEMINI);
    h.executor(&exec);
    // Upstream's first half checks the registry's own epochs; the client is
    // left registered.
    h.models.register(ID, &[MODEL]);

    h.add(active(ID, GEMINI), &[]);
    let (epoch1, _) = h.versions(ID);
    assert!(epoch1 >= 1, "expected RegistrationEpoch >= 1, got {epoch1}");

    assert_eq!(
        pick_by_call(&h, &exec, GEMINI, MODEL)
            .await
            .expect("initial pick"),
        ID
    );

    h.manager.remove(ID);
    assert!(
        pick_by_call(&h, &exec, GEMINI, MODEL).await.is_err(),
        "expected pick to fail after remove"
    );

    h.add(active(ID, GEMINI), &[]);
    let (epoch2, _) = h.versions(ID);
    assert!(epoch2 > epoch1, "epoch2={epoch2}, epoch1={epoch1}");

    // The delayed old-epoch disabled snapshot can't reach the scheduler here.
    assert_eq!(
        pick_by_call(&h, &exec, GEMINI, MODEL)
            .await
            .expect("pick after re-register"),
        ID
    );
}

// Test 38
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_reconcile_concurrent_client_registration_epoch_change_preserves_cooldown()
 {
    const ID: &str = "auth-reconcile-epoch-change-test";
    const MODEL_A: &str = "gemini-route-model-a";
    const MODEL_B: &str = "gemini-route-model-b";
    const MODEL_C: &str = "gemini-route-model-c";
    let h = Harness::new(Settings::default());
    h.executor(&echo(GEMINI));
    h.add(active(ID, GEMINI), &[MODEL_A, MODEL_B]);

    h.manager
        .mark_result(&rate_limited(ID, GEMINI, MODEL_B, MODEL_B, "429", MIN30));

    h.models.register(ID, &[MODEL_B, MODEL_C]);
    h.manager.reconcile_registry_model_states(ID);

    assert!(suspended(&h, ID, MODEL_B), "modelB should stay suspended");
    assert!(
        !suspended(&h, ID, MODEL_C),
        "new modelC should not be suspended"
    );
    assert!(
        state(&h, ID, MODEL_A).is_none(),
        "modelA should have been pruned"
    );
    let state_b = state(&h, ID, MODEL_B);
    assert!(
        state_b
            .as_ref()
            .is_some_and(|s| is_model_state_active_cooldown(s, h.now())),
        "expected modelB to keep its cooldown, got {state_b:?}"
    );
}

// Test 39
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_concurrent_high_load_stability_epoch_and_reconcile() {
    const ID: &str = "auth-high-load-stability";
    const ITERS: usize = 50;

    async fn reconciler(h: &Harness) {
        for _ in 0..ITERS {
            h.manager.reconcile_registry_model_states(ID);
            tokio::task::yield_now().await;
        }
    }
    async fn marker(h: &Harness, worker: usize) {
        let model = if worker % 2 == 1 {
            "model-stable-2"
        } else {
            "model-stable-1"
        };
        for j in 0..ITERS {
            if j % 2 == 0 {
                h.manager
                    .mark_result(&rate_limited(ID, GEMINI, model, model, "429", MIN10));
            } else {
                h.manager.mark_result(&succeeded(ID, GEMINI, model, model));
            }
            tokio::task::yield_now().await;
        }
    }
    async fn registrar(h: &Harness) {
        for j in 0..ITERS {
            if j % 2 == 0 {
                h.models.register(ID, &["model-stable-1", "model-stable-2"]);
            } else {
                h.models.register(ID, &["model-stable-2", "model-stable-3"]);
            }
            tokio::task::yield_now().await;
        }
    }
    async fn picker(h: &Harness) {
        for _ in 0..ITERS {
            let _ = h
                .manager
                .execute(&providers(&[GEMINI]), request("model-stable-2"), options())
                .await;
            tokio::task::yield_now().await;
        }
    }

    let h = Harness::new(Settings::default());
    h.executor(&echo(GEMINI));
    h.add(active(ID, GEMINI), &[]);
    h.models.register(ID, &["model-stable-1", "model-stable-2"]);

    tokio::join!(
        reconciler(&h),
        reconciler(&h),
        reconciler(&h),
        reconciler(&h),
        marker(&h, 0),
        marker(&h, 1),
        marker(&h, 2),
        marker(&h, 3),
        registrar(&h),
        picker(&h),
    );

    h.models.register(ID, &["model-stable-final"]);
    let _ = h.manager.reset_quota(ID);
    h.manager.reconcile_registry_model_states(ID);

    assert!(
        !suspended(&h, ID, "model-stable-final"),
        "expected model-stable-final to not be suspended after reset and reconcile"
    );
}

// Test 40
#[tokio::test(start_paused = true)]
async fn manager_update_rejects_stale_registration_epoch() {
    const ID: &str = "auth-update-stale-epoch-test";
    let h = Harness::new(Settings::default());
    h.executor(&echo(GEMINI));

    h.add(active(ID, GEMINI), &[]);
    let (epoch1, _) = h.versions(ID);
    assert!(
        epoch1 >= 1,
        "expected registration epoch >= 1, got {epoch1}"
    );

    h.manager.remove(ID);
    let registered2 = h.add(active(ID, GEMINI), &[]);
    let (epoch2, generation2) = h.versions(ID);
    assert!(
        epoch2 > epoch1,
        "expected epoch2 ({epoch2}) > epoch1 ({epoch1})"
    );

    // An update can't carry a stale epoch here, so upstream's rejected stale
    // update has no counterpart; the valid one takes the live epoch.
    h.manager
        .update((*registered2).clone())
        .expect("update with the current epoch")
        .expect("updated auth");
    let (epoch, generation) = h.versions(ID);
    assert_eq!(epoch, epoch2);
    assert!(
        generation > generation2,
        "expected Generation to increase on valid update: {generation} <= {generation2}"
    );
}

// Test 41
#[tokio::test(start_paused = true)]
async fn manager_remove_increments_auth_epoch_and_rejects_delayed_old_epoch_snapshot() {
    const ID: &str = "auth-remove-tombstone-epoch-test";
    const MODEL: &str = "gemini-remove-tombstone-epoch-model";
    let h = Harness::new(Settings::default());
    let exec = echo(GEMINI);
    h.executor(&exec);
    h.add(active(ID, GEMINI), &[MODEL]);
    let (epoch1, _) = h.versions(ID);

    assert_eq!(
        pick_by_call(&h, &exec, GEMINI, MODEL)
            .await
            .expect("initial pick"),
        ID
    );

    h.manager.remove(ID);
    let current_epoch = h.manager.lock().epochs[ID];
    assert!(
        current_epoch > epoch1,
        "expected the epoch to increment on Remove: {current_epoch} <= {epoch1}"
    );

    // The delayed old-epoch snapshot can't reach the scheduler here.
    assert!(
        pick_by_call(&h, &exec, GEMINI, MODEL).await.is_err(),
        "expected pick to fail after remove"
    );
}

// Test 43
#[tokio::test(start_paused = true)]
async fn manager_scheduler_delayed_old_epoch_remove_tombstone_rejected_and_new_auth_schedulable() {
    const ID: &str = "auth-old-tombstone-rejection-test";
    const MODEL: &str = "gemini-old-tombstone-rejection-model";
    let h = Harness::new(Settings::default());
    let exec = echo(GEMINI);
    h.executor(&exec);
    h.add(active(ID, GEMINI), &[MODEL]);
    let (epoch1, _) = h.versions(ID);

    assert_eq!(
        pick_by_call(&h, &exec, GEMINI, MODEL)
            .await
            .expect("pick 1"),
        ID
    );

    h.manager.remove(ID);
    let removal_epoch = h.manager.lock().epochs[ID];
    assert!(
        removal_epoch > epoch1,
        "expected removal epoch > epoch1: {removal_epoch} <= {epoch1}"
    );
    assert!(
        pick_by_call(&h, &exec, GEMINI, MODEL).await.is_err(),
        "expected pick to fail after removal"
    );

    h.add(active(ID, GEMINI), &[]);
    let (epoch2, _) = h.versions(ID);
    assert!(
        epoch2 > removal_epoch,
        "expected re-registered epoch > removal epoch: {epoch2} <= {removal_epoch}"
    );
    assert_eq!(
        pick_by_call(&h, &exec, GEMINI, MODEL)
            .await
            .expect("pick 2"),
        ID
    );

    // The delayed old tombstone can't reach the scheduler here; the live
    // registration keeps its epoch and a non-zero generation.
    let (epoch, generation) = h.versions(ID);
    assert_eq!(epoch, epoch2);
    assert!(generation > 0, "generation zeroed");
    assert_eq!(
        pick_by_call(&h, &exec, GEMINI, MODEL)
            .await
            .expect("pick 3"),
        ID
    );
}

/// A registry whose registration of a client moves on each time the manager
/// checks its epoch, while `shifting` is set.
#[derive(Default)]
struct ShiftingModels {
    inner: FakeModels,
    shifting: AtomicBool,
}

impl ClientModels for ShiftingModels {
    fn models_for_client(&self, client_id: &str) -> Vec<String> {
        self.inner.models_for_client(client_id)
    }

    fn models_and_epoch_for_client(&self, client_id: &str) -> (Vec<String>, u64) {
        self.inner.models_and_epoch_for_client(client_id)
    }

    fn client_registration_epoch(&self, client_id: &str) -> u64 {
        if self.shifting.load(Ordering::SeqCst) {
            let models = self.inner.models_for_client(client_id);
            let models: Vec<&str> = models.iter().map(String::as_str).collect();
            self.inner.register(client_id, &models);
        }
        self.inner.client_registration_epoch(client_id)
    }

    fn apply_client_model_projections(
        &self,
        client_id: &str,
        epoch: u64,
        generation: u64,
        projections: &[ModelProjection],
    ) -> bool {
        self.inner
            .apply_client_model_projections(client_id, epoch, generation, projections)
    }
}

// Test 44
#[tokio::test(start_paused = true)]
async fn manager_no_fork_alias_reconcile_exhausted_retry_on_epoch_mismatch_preserves_state_and_no_stale_snapshot()
 {
    const ID: &str = "auth-reconcile-exhaust-test";
    const MODEL_A: &str = "gemini-reconcile-exhaust-model-a";
    const MODEL_B: &str = "gemini-reconcile-exhaust-model-b";
    let clock = TestClock::new();
    let models = Arc::new(ShiftingModels::default());
    let manager = Manager::with_clock(Settings::default(), models.clone(), None, clock.clock());
    manager.register_executor(echo(GEMINI));
    models.inner.register(ID, &[MODEL_A, MODEL_B]);
    manager.register(active(ID, GEMINI)).expect("register auth");

    manager.mark_result(&rate_limited(ID, GEMINI, MODEL_B, MODEL_B, "429", MIN30));
    let initial_gen = manager.lock().auths[ID].generation;
    let publishes = models.inner.published().len();

    models.shifting.store(true, Ordering::SeqCst);
    manager.reconcile_registry_model_states(ID);
    models.shifting.store(false, Ordering::SeqCst);

    let after = manager.get(ID).expect("auth was lost in manager");
    let state_b = after.model_states.get(MODEL_B);
    assert!(
        state_b.is_some_and(|s| is_model_state_active_cooldown(s, clock.now())),
        "expected modelB to keep its cooldown after retry exhaustion, got {state_b:?}"
    );
    let generation = manager.lock().auths[ID].generation;
    assert!(
        generation >= initial_gen,
        "unexpected generation decrease: {generation} < {initial_gen}"
    );
    // Every attempt saw a newer registration, so nothing was committed or
    // published.
    assert_eq!(generation, initial_gen);
    assert_eq!(models.inner.published().len(), publishes);

    manager.reconcile_registry_model_states(ID);
    let final_auth = manager.get(ID).expect("auth");
    let state_b = final_auth.model_states.get(MODEL_B);
    assert!(
        state_b.is_some_and(|s| is_model_state_active_cooldown(s, clock.now())),
        "expected modelB to keep its cooldown after a clean reconcile, got {state_b:?}"
    );
}

// Test 45
#[tokio::test(start_paused = true)]
async fn manager_load_monotonic_epoch_assignment_and_removal_tombstones() {
    const ID1: &str = "auth-load-epoch-1";
    const ID2: &str = "auth-load-epoch-2";
    const MODEL: &str = "gemini-load-epoch-test-model";
    let h = Harness::with_store(Settings::default());
    // Upstream stores these with epochs 5 and 3; `Auth` has no epoch here.
    h.store.put(active(ID1, GEMINI));
    h.store.put(active(ID2, GEMINI));
    let exec = echo(GEMINI);
    h.executor(&exec);
    h.models.register(ID1, &[MODEL]);
    h.models.register(ID2, &[MODEL]);

    h.manager.load().expect("load");
    assert!(h.manager.get(ID1).is_some() && h.manager.get(ID2).is_some());
    // Upstream: 6 and 4, one past the stored epochs.
    assert_eq!(h.versions(ID1), (1, 1));
    assert_eq!(h.versions(ID2), (1, 1));

    let mut picked = BTreeSet::new();
    for _ in 0..4 {
        picked.insert(pick_by_call(&h, &exec, GEMINI, MODEL).await.expect("pick"));
    }
    assert!(
        picked.contains(ID1) && picked.contains(ID2),
        "expected both auths picked, got {picked:?}"
    );

    AuthStore::delete(&*h.store, ID2).expect("delete");
    h.manager.load().expect("load 2");

    // Upstream: 7.
    assert_eq!(h.versions(ID1).0, 2);
    assert!(h.manager.get(ID2).is_none(), "auth2 should be gone");
    // Upstream: 5.
    assert_eq!(h.manager.lock().epochs[ID2], 2);

    // The stale snapshot of auth2 can't reach the scheduler here.
    for _ in 0..3 {
        assert_eq!(
            pick_by_call(&h, &exec, GEMINI, MODEL)
                .await
                .expect("pick after reload"),
            ID1
        );
    }
}
