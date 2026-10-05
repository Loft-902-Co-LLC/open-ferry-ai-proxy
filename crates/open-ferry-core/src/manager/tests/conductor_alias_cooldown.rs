// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_alias_cooldown_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! An OAuth model alias is checked against its target model's cooldown: a
//! cooldown on another model doesn't block it, a cooldown on the target does,
//! and a credential-scoped quota failure on the target fails over to the next
//! credential.
//!
//! Deviations from upstream:
//! - `SelectAuth` isn't ported; the "select" path and
//!   `TestManagerSelectAuth_ModelAliasRequestNotBlockedByOtherModelQuotaCooldown`
//!   pick through `Selection::pick_next_mixed` for the one provider, which
//!   takes upstream's legacy pick for an aliased model as `SelectAuth` does.
//! - The aliases' `Fork: true` is dropped: forking is a registry concern and
//!   the tests register the alias and target models themselves.
//! - `RefreshSchedulerEntry` has no counterpart: picks read the credentials
//!   as they are.
//! - Upstream's per-selector cases are the three routing strategies.
//! - `*modelCooldownError` is an `ExecError` of kind `ModelCooldown`, whose
//!   JSON body names the model.

use super::support::*;

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::TimeDelta;

use crate::auth::{Auth, AuthError, ModelState, QuotaState, Status};
use crate::exec::{Dispatcher, ErrorKind, ExecError};
use crate::manager::cooldown::update_aggregated_availability;
use crate::manager::models::Resolver;
use crate::manager::select::{PickArgs, Selection};
use crate::manager::{CallResult, ModelAlias, RoutingStrategy, Settings};

/// Picks a credential for `model` on `provider` as upstream's `SelectAuth`
/// does: one pick, nothing tried yet, not pinned.
fn select_auth(h: &Harness, provider: &str, model: &str) -> Result<Arc<Auth>, ExecError> {
    let now = h.now();
    let (settings, oauth) = h.manager.resolver_parts();
    let mut guard = h.manager.lock();
    let state = &mut *guard;
    let selection = Selection {
        auths: &state.auths,
        executors: &state.executors,
        models: &*h.models,
        resolver: Resolver {
            settings: &settings,
            oauth: &oauth,
        },
        strategy: settings.routing_strategy,
        now,
    };
    let tried = HashSet::new();
    let args = PickArgs {
        model,
        pinned: "",
        downstream_websocket: false,
        tried: &tried,
    };
    selection
        .pick_next_mixed(&mut state.selector, &providers(&[provider]), &args)
        .map(|picked| picked.auth)
}

fn alias_settings(provider: &str, name: &str, alias: &str) -> Settings {
    Settings {
        oauth_model_alias: BTreeMap::from([(
            provider.to_owned(),
            vec![ModelAlias {
                name: name.into(),
                alias: alias.into(),
                force_mapping: false,
            }],
        )]),
        ..Settings::default()
    }
}

#[tokio::test(start_paused = true)]
async fn manager_alias_quota_failover_with_unobserved_target_model() {
    const ROUTE_MODEL: &str = "quota-route";
    const TARGET_MODEL: &str = "quota-target";
    const OTHER_MODEL: &str = "quota-other";
    let strategies = [
        ("round-robin", RoutingStrategy::RoundRobin),
        ("weighted-round-robin", RoutingStrategy::Weighted),
        ("fill-first", RoutingStrategy::FillFirst),
    ];
    for (strategy_name, strategy) in strategies {
        for path in ["select", "execute", "stream"] {
            let name = format!("{strategy_name}/{path}");
            let h = Harness::new(Settings {
                request_retry: 3,
                max_retry_interval: Duration::from_secs(30),
                routing_strategy: strategy,
                ..alias_settings("codex", TARGET_MODEL, ROUTE_MODEL)
            });
            let high_id = format!("alias-quota-high-{name}");
            let low_id = format!("alias-quota-low-{name}");
            for (id, priority) in [(&high_id, "4"), (&low_id, "3")] {
                let mut candidate = auth(id, "codex");
                candidate.status = Status::Active;
                candidate
                    .attributes
                    .insert("priority".into(), priority.into());
                candidate.attributes.insert("weight".into(), "1".into());
                h.add(candidate, &[ROUTE_MODEL, TARGET_MODEL, OTHER_MODEL]);
            }
            let retry_after = Duration::from_secs(60 * 60);
            h.manager.mark_result(&CallResult {
                auth_id: high_id.clone(),
                provider: "codex".into(),
                model: OTHER_MODEL.into(),
                error: Some(AuthError {
                    http_status: 429,
                    message: "other model rate limit".into(),
                    ..AuthError::default()
                }),
                retry_after: Some(retry_after),
                ..CallResult::default()
            });
            let high = h.get(&high_id);
            assert!(
                high.unavailable && !high.model_states.contains_key(TARGET_MODEL),
                "{name}: expected aggregate cooldown with no state yet for the requested target"
            );

            let exhausted = high_id.clone();
            let healthy = low_id.clone();
            let executor = FakeExecutor::with("codex", move |call: &Call| {
                if call.auth_id == exhausted {
                    let mut err = ExecError::upstream(429, "account quota exhausted")
                        .with_credential_scoped();
                    err.retry_after = Some(retry_after);
                    Reply::Err(err)
                } else {
                    Reply::ok(healthy.clone())
                }
            });
            h.executor(&executor);

            match path {
                "select" => {
                    let selected = select_auth(&h, "codex", ROUTE_MODEL)
                        .unwrap_or_else(|err| panic!("{name}: select: {err}"));
                    assert!(
                        selected.id == high_id && selected.unavailable && selected.quota.exceeded,
                        "{name}: selection must preserve the selected auth's actual state: id={} unavailable={} quota={:?}",
                        selected.id,
                        selected.unavailable,
                        selected.quota
                    );
                    continue;
                }
                "execute" => {
                    let response = h
                        .manager
                        .execute(&providers(&["codex"]), request(ROUTE_MODEL), options())
                        .await
                        .unwrap_or_else(|err| panic!("{name}: execute: {err}"));
                    assert_eq!(
                        String::from_utf8_lossy(&response.payload),
                        low_id,
                        "{name}: want healthy lower-priority account"
                    );
                }
                _ => {
                    let mut opts = options();
                    opts.stream = true;
                    let stream = h
                        .manager
                        .execute_stream(&providers(&["codex"]), request(ROUTE_MODEL), opts)
                        .await
                        .unwrap_or_else(|err| panic!("{name}: stream: {err}"));
                    let (chunks, err) = collect(stream).await;
                    assert!(err.is_none(), "{name}: stream ended with {err:?}");
                    for chunk in &chunks {
                        assert_eq!(
                            chunk, &low_id,
                            "{name}: want healthy lower-priority account"
                        );
                    }
                }
            }
            let calls = executor.calls();
            for call in &calls {
                assert_eq!(
                    call.model, TARGET_MODEL,
                    "{name}: upstream model = {}, want {TARGET_MODEL}",
                    call.model
                );
            }
            let attempts: Vec<&str> = calls.iter().map(|c| c.auth_id.as_str()).collect();
            assert_eq!(
                attempts,
                [high_id.as_str(), low_id.as_str()],
                "{name}: want exhausted account then healthy account"
            );
        }
    }
}

const PROVIDER: &str = "antigravity";
const REQUEST_MODEL: &str = "gemini-3.6-flash";
const TARGET_MODEL: &str = "gemini-3.6-flash-high";
const IMAGE_MODEL: &str = "gemini-3.1-flash-image";

fn quota_state(next: crate::auth::Timestamp) -> ModelState {
    ModelState {
        status: Status::Error,
        unavailable: true,
        next_retry_after: Some(next),
        quota: QuotaState {
            exceeded: true,
            reason: "quota".into(),
            next_recover_at: Some(next),
            ..QuotaState::default()
        },
        ..ModelState::default()
    }
}

/// A manager with `aliasRoutingExecutor` (answers with the model it got) and
/// the request-model alias for the target model.
fn alias_manager() -> (Harness, Arc<FakeExecutor>) {
    let h = Harness::new(alias_settings(PROVIDER, TARGET_MODEL, REQUEST_MODEL));
    let executor = FakeExecutor::with(PROVIDER, |call: &Call| Reply::ok(call.model.clone()));
    h.executor(&executor);
    (h, executor)
}

/// A credential whose target model is active and whose image model is in a
/// quota cooldown for an hour, with its aggregate state worked out.
fn image_cooled_auth(h: &Harness, id: &str) -> Auth {
    let now = h.now();
    let next = now + TimeDelta::hours(1);
    let mut a = auth(id, PROVIDER);
    a.status = Status::Active;
    a.model_states.insert(
        TARGET_MODEL.into(),
        ModelState {
            status: Status::Active,
            ..ModelState::default()
        },
    );
    a.model_states.insert(IMAGE_MODEL.into(), quota_state(next));
    update_aggregated_availability(&mut a, now);
    assert!(
        a.quota.exceeded,
        "precondition failed: quota.exceeded should be true after update_aggregated_availability"
    );
    assert!(
        !a.unavailable,
        "precondition failed: unavailable should be false since the target model is active"
    );
    a
}

#[tokio::test(start_paused = true)]
async fn manager_execute_model_alias_request_not_blocked_by_other_model_quota_cooldown() {
    let (h, _executor) = alias_manager();
    let a = image_cooled_auth(&h, "antigravity-auth-1");
    let id = a.id.clone();
    h.add(a, &[]);
    h.models
        .register(&id, &[REQUEST_MODEL, TARGET_MODEL, IMAGE_MODEL]);

    let resp = h
        .manager
        .execute(&providers(&[PROVIDER]), request(REQUEST_MODEL), options())
        .await
        .unwrap_or_else(|err| panic!("execute error = {err}, want success"));
    assert_eq!(String::from_utf8_lossy(&resp.payload), TARGET_MODEL);
}

#[tokio::test(start_paused = true)]
async fn manager_select_auth_model_alias_request_not_blocked_by_other_model_quota_cooldown() {
    let (h, _executor) = alias_manager();
    let a = image_cooled_auth(&h, "antigravity-auth-2");
    let id = a.id.clone();
    h.add(a, &[]);
    h.models
        .register(&id, &[REQUEST_MODEL, TARGET_MODEL, IMAGE_MODEL]);

    let selected = select_auth(&h, PROVIDER, REQUEST_MODEL)
        .unwrap_or_else(|err| panic!("select error = {err}, want success"));
    assert_eq!(selected.id, id);
}

#[tokio::test(start_paused = true)]
async fn manager_execute_model_alias_request_blocked_when_target_model_in_quota_cooldown() {
    let (h, _executor) = alias_manager();
    let now = h.now();
    let mut a = auth("antigravity-auth-3", PROVIDER);
    a.status = Status::Active;
    a.model_states
        .insert(TARGET_MODEL.into(), quota_state(now + TimeDelta::hours(1)));
    update_aggregated_availability(&mut a, now);
    let id = a.id.clone();
    h.add(a, &[]);
    h.models.register(&id, &[REQUEST_MODEL, TARGET_MODEL]);

    let err = match h
        .manager
        .execute(&providers(&[PROVIDER]), request(REQUEST_MODEL), options())
        .await
    {
        Ok(_) => panic!("execute error = none, want cooldown error"),
        Err(err) => err,
    };
    assert_eq!(
        err.kind,
        ErrorKind::ModelCooldown,
        "execute error = {err}, want a model cooldown error"
    );
    let body: serde_json::Value =
        serde_json::from_str(&err.to_string()).expect("cooldown error body is JSON");
    assert_eq!(body["error"]["model"], REQUEST_MODEL);
}
