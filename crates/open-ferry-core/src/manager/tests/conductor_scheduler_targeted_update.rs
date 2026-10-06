// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_scheduler_targeted_update_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Results that touch one model or the whole credential, as picks and the
//! published model projections see them: a model failure blocks only that
//! model, a credential-wide failure blocks every model, a success on one
//! model recovers a credential-wide failure, and models the registry adds
//! are picked after the next result.
//!
//! Deviations from upstream:
//! - Upstream checks the scheduler's model shards (entry states, entry
//!   pointers left alone). The port has no shards; each test checks the
//!   picks and the published projections (`suspended`) those shards stand
//!   for. The exact errors are upstream's, taken from running the same picks
//!   against upstream.
//! - Picks are calls through the manager (`execute`), so each test
//!   registers an executor for its provider. A call also records its
//!   success, which upstream's scheduler-only picks don't; where that
//!   matters (`scheduler_credential_level_recovery_via_model_success_updates_all_shards`)
//!   the first picks go to the selector directly (`pick_only`).
//! - `scheduler_mark_result_out_of_order_cross_model_updates` and
//!   `scheduler_mark_result_out_of_order_credential_scoped_failure_updates_all_shards`:
//!   upstream hands the scheduler two result snapshots in the wrong order.
//!   Picks here read the live credential, so there's no order to get wrong;
//!   the credential takes the newest snapshot's state (`update`) and the
//!   picks upstream checks must fail.
//! - Dropped: `TestScheduler_MarkResult_CredentialScopedReusesModelSet`
//!   (whether the index reuses its supported-model map; index bookkeeping
//!   with nothing observable).
//! - Dropped: `TestScheduler_StaleDisabledSnapshotDoesNotRemoveActiveAuth`
//!   and `TestScheduler_CredentialLevelRecoveryViaModelSuccess_OutOfOrder`
//!   (a stale snapshot handed straight to the index; the manager's
//!   credential never changes, and the port has no index to hand it to).
//! - Dropped: `BenchmarkScheduler_TargetedMarkResultHighConcurrency`
//!   (benchmark).

use super::support::*;

use std::collections::HashSet;
use std::time::Duration;

use chrono::TimeDelta;

use crate::auth::{AuthError, ModelState, QuotaState, Status};
use crate::exec::{ErrorKind, ExecError};
use crate::manager::models::Resolver;
use crate::manager::select::{PickArgs, Selection};
use crate::manager::{CallResult, Settings};

/// Picks a credential for `model` over `provider` without a call, so no
/// result is recorded (upstream's `scheduler.pickSingle`).
fn pick_only(h: &Harness, provider: &str, model: &str) -> Result<String, ExecError> {
    let now = h.now();
    let mut guard = h.manager.lock();
    let state = &mut *guard;
    let selection = Selection {
        auths: &state.auths,
        executors: &state.executors,
        models: h.models.as_ref(),
        resolver: Resolver {
            settings: &state.settings,
            oauth: &state.oauth,
        },
        strategy: state.settings.routing_strategy,
        now,
    };
    let tried = HashSet::new();
    let args = PickArgs {
        model,
        pinned: "",
        downstream_websocket: false,
        eligibility: Default::default(),
        tried: &tried,
    };
    selection
        .pick_next_mixed(&mut state.selector, &providers(&[provider]), &args)
        .map(|picked| picked.auth.id.clone())
}

/// An active credential `id` for `provider`, registered with `models`, and
/// an executor for the provider.
fn setup(h: &Harness, id: &str, provider: &str, models: &[&str]) -> std::sync::Arc<FakeExecutor> {
    let executor = FakeExecutor::new(provider);
    h.executor(&executor);
    let mut active = auth(id, provider);
    active.status = Status::Active;
    h.add(active, models);
    executor
}

/// A failed result on `model` (empty for the whole credential).
fn failure(
    auth_id: &str,
    provider: &str,
    model: &str,
    credential_scope: bool,
    (code, message, http_status): (&str, &str, u16),
) -> CallResult {
    CallResult {
        auth_id: auth_id.into(),
        provider: provider.into(),
        model: model.into(),
        success: false,
        credential_scope,
        error: Some(AuthError {
            code: code.into(),
            message: message.into(),
            http_status,
            ..AuthError::default()
        }),
        ..CallResult::default()
    }
}

const INTERNAL_SERVER_ERROR: (&str, &str, u16) =
    ("internal_server_error", "500 Internal Server Error", 500);

fn success(auth_id: &str, provider: &str, model: &str) -> CallResult {
    CallResult {
        auth_id: auth_id.into(),
        provider: provider.into(),
        model: model.into(),
        success: true,
        ..CallResult::default()
    }
}

fn suspended(h: &Harness, id: &str, model: &str) -> bool {
    h.models
        .projection(id, model)
        .unwrap_or_else(|| panic!("no projection for {model}"))
        .suspended
}

/// Asserts `err` is upstream's error after a 500 on the only credential for
/// the model: unavailable for the 60s transient cooldown.
fn assert_unavailable_after_500(err: &ExecError) {
    assert_eq!(err.kind, ErrorKind::AuthUnavailable, "{err}");
    assert_eq!(err.status, 503);
    assert!(!err.terminal_auth);
    assert_eq!(err.retry_after, Some(Duration::from_secs(60)));
    assert_eq!(
        err.to_string(),
        "auth_unavailable: no auth available (last upstream error: internal_server_error: 500 Internal Server Error)"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_targeted_model_shard_update() {
    let h = Harness::new(Settings::default());
    let id = "auth-targeted-test";
    let provider = "custom-prov";
    let executor = setup(&h, id, provider, &["model-a", "model-b", "model-c"]);
    for model in ["model-a", "model-b", "model-c"] {
        let picked = pick_by_call(&h, &executor, provider, model)
            .await
            .unwrap_or_else(|err| panic!("pick({model}): {err}"));
        assert_eq!(picked, id, "{model}");
    }

    h.manager.mark_result(&failure(
        id,
        provider,
        "model-a",
        false,
        INTERNAL_SERVER_ERROR,
    ));

    assert!(suspended(&h, id, "model-a"), "model-a must be blocked");
    assert!(!suspended(&h, id, "model-b"), "model-b must stay ready");
    assert!(!suspended(&h, id, "model-c"), "model-c must stay ready");
    let err = pick_by_call(&h, &executor, provider, "model-a")
        .await
        .expect_err("model-a is cooling down");
    assert_unavailable_after_500(&err);
    for model in ["model-b", "model-c"] {
        let picked = pick_by_call(&h, &executor, provider, model)
            .await
            .unwrap_or_else(|err| panic!("{model} must stay ready: {err}"));
        assert_eq!(picked, id, "{model}");
    }
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_success_targeted_update() {
    let h = Harness::new(Settings::default());
    let id = "auth-success-targeted-test";
    let provider = "custom-prov-succ";
    let executor = setup(&h, id, provider, &["model-1", "model-2"]);
    for model in ["model-1", "model-2"] {
        let picked = pick_by_call(&h, &executor, provider, model)
            .await
            .unwrap_or_else(|err| panic!("pick({model}): {err}"));
        assert_eq!(picked, id, "{model}");
    }

    h.manager.mark_result(&success(id, provider, "model-1"));

    assert!(!suspended(&h, id, "model-1"));
    assert!(!suspended(&h, id, "model-2"));
    let picked = pick_by_call(&h, &executor, provider, "model-2")
        .await
        .expect("model-2 is untouched");
    assert_eq!(picked, id);
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_credential_scoped_updates_all_shards() {
    let h = Harness::new(Settings::default());
    let id = "auth-cred-scope-test";
    let provider = "custom-prov-cred";
    let executor = setup(&h, id, provider, &["model-x", "model-y"]);
    for model in ["model-x", "model-y"] {
        let picked = pick_by_call(&h, &executor, provider, model)
            .await
            .unwrap_or_else(|err| panic!("pick({model}): {err}"));
        assert_eq!(picked, id, "{model}");
    }

    h.manager.mark_result(&failure(
        id,
        provider,
        "model-x",
        true,
        ("quota_exceeded", "429 Quota Exceeded", 429),
    ));

    for model in ["model-x", "model-y"] {
        assert!(
            suspended(&h, id, model),
            "{model} must not be ready after credential-scoped error"
        );
        let err = pick_by_call(&h, &executor, provider, model)
            .await
            .expect_err(model);
        assert_eq!(err.kind, ErrorKind::ModelCooldown, "{model}: {err}");
        assert_eq!(err.status, 429);
        assert_eq!(err.retry_after, Some(Duration::from_secs(1)));
        assert_eq!(
            err.to_string(),
            format!(
                r#"{{"error":{{"code":"model_cooldown","last_upstream_error":"quota_exceeded: 429 Quota Exceeded","message":"All credentials for model {model} are cooling down via provider custom-prov-cred (last error: quota_exceeded: 429 Quota Exceeded)","model":"{model}","provider":"custom-prov-cred","reset_seconds":1,"reset_time":"1s"}}}}"#
            )
        );
    }
}

#[tokio::test(start_paused = true)]
async fn scheduler_mark_result_out_of_order_cross_model_updates() {
    let h = Harness::new(Settings::default());
    let id = "auth-ooo-test";
    let provider = "custom-prov-ooo";
    let executor = setup(&h, id, provider, &["model-p", "model-q"]);
    for model in ["model-p", "model-q"] {
        let picked = pick_by_call(&h, &executor, provider, model)
            .await
            .unwrap_or_else(|err| panic!("pick({model}): {err}"));
        assert_eq!(picked, id, "{model}");
    }

    // The newest snapshot (upstream's generation 2) holds both failures.
    let blocked = ModelState {
        unavailable: true,
        status: Status::Error,
        next_retry_after: Some(h.now() + TimeDelta::minutes(10)),
        ..ModelState::default()
    };
    let mut snapshot = (*h.get(id)).clone();
    snapshot.model_states = [
        ("model-p".to_owned(), blocked.clone()),
        ("model-q".to_owned(), blocked),
    ]
    .into();
    h.manager
        .update(snapshot)
        .expect("update")
        .expect("a registered credential");

    for model in ["model-p", "model-q"] {
        let err = pick_by_call(&h, &executor, provider, model)
            .await
            .expect_err(model);
        assert_eq!(err.kind, ErrorKind::AuthUnavailable, "{model}: {err}");
        assert_eq!(err.status, 503);
        assert_eq!(err.retry_after, Some(Duration::from_secs(600)));
        assert_eq!(err.to_string(), "auth_unavailable: no auth available");
    }
}

#[tokio::test(start_paused = true)]
async fn scheduler_mark_result_empty_model_shard_updated() {
    let h = Harness::new(Settings::default());
    let id = "auth-empty-shard-test";
    let provider = "custom-prov-empty";
    let executor = setup(&h, id, provider, &["only-model"]);
    for model in ["", "only-model"] {
        let picked = pick_by_call(&h, &executor, provider, model)
            .await
            .unwrap_or_else(|err| panic!("pick({model:?}): {err}"));
        assert_eq!(picked, id, "{model:?}");
    }

    h.manager.mark_result(&failure(
        id,
        provider,
        "only-model",
        false,
        INTERNAL_SERVER_ERROR,
    ));

    let err = pick_by_call(&h, &executor, provider, "")
        .await
        .expect_err("no model of the credential is available");
    assert_unavailable_after_500(&err);
}

#[tokio::test(start_paused = true)]
async fn scheduler_model_registry_epoch_invalidates_cache() {
    let h = Harness::new(Settings::default());
    let id = "auth-reg-epoch-test";
    let provider = "custom-prov-reg-epoch";
    let executor = setup(&h, id, provider, &["model-1"]);
    pick_by_call(&h, &executor, provider, "model-1")
        .await
        .expect("pick(model-1)");
    let err = pick_by_call(&h, &executor, provider, "model-2")
        .await
        .expect_err("model-2 should not be supported initially");
    assert_eq!(err.kind, ErrorKind::AuthNotFound);
    assert_eq!(err.to_string(), "auth_not_found: no auth available");

    h.models.register(id, &["model-1", "model-2"]);
    h.manager.mark_result(&success(id, provider, "model-1"));

    let published = h.models.published();
    let last = published.last().expect("the result was published");
    assert_eq!(last.epoch, 2, "published against the new registry epoch");
    let models: Vec<&str> = last
        .projections
        .iter()
        .map(|p| p.model_id.as_str())
        .collect();
    assert_eq!(models, ["model-1", "model-2"]);
    let picked = pick_by_call(&h, &executor, provider, "model-2")
        .await
        .expect("pick(model-2) after the registry epoch changed");
    assert_eq!(picked, id);
}

#[tokio::test(start_paused = true)]
async fn scheduler_mark_result_out_of_order_credential_scoped_failure_updates_all_shards() {
    let h = Harness::new(Settings::default());
    let id = "auth-ooo-cred-test";
    let provider = "custom-prov-ooo-cred";
    let executor = setup(&h, id, provider, &["m-ooo-1", "m-ooo-2"]);
    for model in ["m-ooo-1", "m-ooo-2"] {
        let picked = pick_by_call(&h, &executor, provider, model)
            .await
            .unwrap_or_else(|err| panic!("pick({model}): {err}"));
        assert_eq!(picked, id, "{model}");
    }

    // Both snapshots carry the credential-wide quota.
    let mut snapshot = (*h.get(id)).clone();
    snapshot.unavailable = true;
    snapshot.quota = QuotaState {
        exceeded: true,
        reason: "credential_quota".into(),
        next_recover_at: Some(h.now() + TimeDelta::hours(1)),
        backoff_level: 0,
    };
    h.manager
        .update(snapshot)
        .expect("update")
        .expect("a registered credential");

    for model in ["m-ooo-1", "m-ooo-2"] {
        let err = pick_by_call(&h, &executor, provider, model)
            .await
            .expect_err(model);
        assert_eq!(err.kind, ErrorKind::ModelCooldown, "{model}: {err}");
        assert_eq!(err.status, 429);
        assert_eq!(err.retry_after, Some(Duration::from_secs(3600)));
        assert_eq!(
            err.to_string(),
            format!(
                r#"{{"error":{{"code":"model_cooldown","message":"All credentials for model {model} are cooling down via provider custom-prov-ooo-cred","model":"{model}","provider":"custom-prov-ooo-cred","reset_seconds":3600,"reset_time":"1h0m0s"}}}}"#
            )
        );
    }
}

#[tokio::test(start_paused = true)]
async fn scheduler_model_registry_epoch_change_syncs_existing_shards() {
    let h = Harness::new(Settings::default());
    let provider = "custom-prov-epoch-sync";
    let executor = FakeExecutor::new(provider);
    h.executor(&executor);
    for (id, model) in [("auth-1", "model-1"), ("auth-2", "model-2")] {
        let mut active = auth(id, provider);
        active.status = Status::Active;
        h.add(active, &[model]);
    }

    assert_eq!(
        pick_by_call(&h, &executor, provider, "model-1")
            .await
            .expect("pick(model-1)"),
        "auth-1"
    );
    // Only auth-2 serves model-2 before the registration change.
    for _ in 0..2 {
        assert_eq!(
            pick_by_call(&h, &executor, provider, "model-2")
                .await
                .expect("pick(model-2)"),
            "auth-2"
        );
    }

    h.models.register("auth-1", &["model-1", "model-2"]);
    h.manager
        .mark_result(&success("auth-1", provider, "model-1"));

    assert!(!suspended(&h, "auth-1", "model-2"));
    let mut picked = Vec::new();
    for _ in 0..2 {
        picked.push(
            pick_by_call(&h, &executor, provider, "model-2")
                .await
                .expect("pick(model-2)"),
        );
    }
    picked.sort();
    assert_eq!(picked, ["auth-1", "auth-2"], "auth-1 must serve model-2");
}

#[tokio::test(start_paused = true)]
async fn scheduler_credential_level_recovery_via_model_success_updates_all_shards() {
    let h = Harness::new(Settings::default());
    let id = "auth-global-recovery-test";
    let provider = "custom-prov-recovery";
    let executor = setup(&h, id, provider, &["m-rec-a", "m-rec-b"]);
    // Without a call: a recorded success would give the credential model
    // states, and with model states a credential-wide 401 doesn't suspend
    // the models' projections (upstream's design too).
    for model in ["m-rec-a", "m-rec-b"] {
        let picked =
            pick_only(&h, provider, model).unwrap_or_else(|err| panic!("pick({model}): {err}"));
        assert_eq!(picked, id, "{model}");
    }

    // 1. A credential-level 401.
    h.manager.mark_result(&failure(
        id,
        provider,
        "",
        true,
        ("unauthorized", "401 Unauthorized", 401),
    ));

    for model in ["m-rec-a", "m-rec-b"] {
        assert!(
            suspended(&h, id, model),
            "{model} must not be ready after 401"
        );
        let err = pick_by_call(&h, &executor, provider, model)
            .await
            .expect_err(model);
        // The 401 cools the credential down for 30 minutes; with a retry
        // pending it isn't terminal.
        assert_eq!(err.kind, ErrorKind::AuthUnavailable, "{model}: {err}");
        assert_eq!(err.status, 503);
        assert!(!err.terminal_auth, "{model}");
        assert_eq!(err.retry_after, Some(Duration::from_secs(30 * 60)));
        assert_eq!(
            err.to_string(),
            "auth_unavailable: no auth available (last upstream error: unauthorized: 401 Unauthorized)"
        );
    }

    // 2. A request for m-rec-a that was in flight succeeds.
    h.manager.mark_result(&success(id, provider, "m-rec-a"));

    assert!(!suspended(&h, id, "m-rec-a"));
    assert!(
        !suspended(&h, id, "m-rec-b"),
        "m-rec-b must be ready after credential-level recovery"
    );
    let picked = pick_by_call(&h, &executor, provider, "m-rec-b")
        .await
        .expect("pick(m-rec-b) after recovery");
    assert_eq!(picked, id);
}
