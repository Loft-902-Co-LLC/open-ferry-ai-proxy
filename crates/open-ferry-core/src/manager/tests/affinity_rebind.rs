// Ported from CLIProxyAPI sdk/cliproxy/auth/session_affinity_priority_test.go
// and sdk/cliproxy/service_executionregistry_test.go (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Quota-only rebinding through the manager's selection and outcome hooks.
//! LCP tests are omitted because the prefix matcher is not ported.

use std::collections::HashSet;
use std::sync::Arc;

use futures_util::StreamExt;

use super::affinity::{headers, session};
use super::support::*;
use crate::auth::{AuthError, Status};
use crate::exec::{Dispatcher, ExecError};
use crate::manager::affinity::Session;
use crate::manager::models::Resolver;
use crate::manager::policy::CredentialPolicy;
use crate::manager::select::{PickArgs, Selection};
use crate::manager::{CallResult, RoutingStrategy, Settings};

const MODEL: &str = "test-model";

fn settings() -> Settings {
    Settings {
        session_affinity: true,
        session_affinity_rebind_on: "quota".into(),
        ..Settings::default()
    }
}

fn setup(settings: Settings) -> Harness {
    let h = Harness::new(settings);
    h.executor(&FakeExecutor::new("codex"));
    for (id, priority) in [("high", "2"), ("mid", "1"), ("low", "0")] {
        let mut a = auth_with_metadata(id, "codex", serde_json::json!({"auth_kind": "oauth"}));
        a.status = Status::Active;
        a.attributes.insert("priority".into(), priority.into());
        h.add(a, &[MODEL]);
    }
    h
}

fn stable() -> Session {
    session(&[("Session-Id", "stable")], "")
}

fn pick(h: &Harness, sess: &Session, single: bool, tried: &HashSet<String>) -> String {
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
        now: h.now(),
    };
    let affinity = state.affinity.as_mut().unwrap();
    let picked = if single {
        selection.pick_next_with_policy(
            &mut state.selector,
            Some(affinity),
            "codex",
            MODEL,
            CredentialPolicy::CodexAlphaSearchV1,
            Some(sess),
        )
    } else {
        selection.pick_next_mixed_sticky(
            &mut state.selector,
            affinity,
            &["codex".into()],
            &PickArgs {
                model: MODEL,
                pinned: "",
                downstream_websocket: false,
                tried,
                eligibility: Default::default(),
            },
            Some(sess),
        )
    };
    picked.expect("pick").auth.id.clone()
}

fn outcome(h: &Harness, sess: &Session, single: bool, id: &str, error: Option<AuthError>) {
    let result = CallResult {
        auth_id: id.into(),
        provider: "codex".into(),
        model: MODEL.into(),
        success: error.is_none(),
        error,
        ..CallResult::default()
    };
    h.manager.mark_result(&result);
    h.manager.lock().affinity.as_mut().unwrap().on_result(
        sess,
        &result,
        if single { "codex" } else { "mixed" },
        h.now(),
    );
}

fn recover(h: &Harness, id: &str) {
    let mut guard = h.manager.lock();
    let auth = Arc::make_mut(&mut guard.auths.get_mut(id).unwrap().auth);
    auth.unavailable = false;
    auth.last_error = None;
    auth.next_retry_after = None;
    auth.quota = Default::default();
    auth.model_states.clear();
    auth.status = Status::Active;
}

fn failure(status: u16, message: &str) -> AuthError {
    AuthError {
        http_status: status,
        message: message.into(),
        ..AuthError::default()
    }
}

// TestManagerSessionAffinityRebindOnQuotaPreservesBindingAcrossTransientFailover.
#[test]
fn affinity_rebind_quota_preserves_binding_across_transient_failover() {
    for single in [false, true] {
        let h = setup(settings());
        let sess = stable();
        let tried = HashSet::new();
        assert_eq!(pick(&h, &sess, single, &tried), "high");
        let mut errors = vec![
            failure(502, "502 overloaded"),
            failure(503, "503 connect error"),
            failure(408, "408 timeout"),
            failure(507, "507 buffer limit"),
        ];
        errors.push(AuthError {
            code: "force_cooldown".into(),
            message: "stream completed without any response data".into(),
            ..AuthError::default()
        });
        for error in errors {
            outcome(&h, &sess, single, "high", Some(error));
            assert_eq!(pick(&h, &sess, single, &tried), "mid");
            outcome(&h, &sess, single, "mid", None);
            recover(&h, "high");
            assert_eq!(pick(&h, &sess, single, &tried), "high");
        }
        for status in [429, 403] {
            let sess = session(&[("Session-Id", &format!("failure-{status}"))], "");
            assert_eq!(pick(&h, &sess, single, &tried), "high");
            outcome(
                &h,
                &sess,
                single,
                "high",
                Some(failure(status, "quota or auth")),
            );
            assert_eq!(pick(&h, &sess, single, &tried), "mid");
            outcome(&h, &sess, single, "mid", None);
            recover(&h, "high");
            assert_eq!(pick(&h, &sess, single, &tried), "mid");
        }
    }
}

// TestSessionAffinitySelectorRebindOnQuotaDirectPickRebindsWhenAuthRemovedOrZeroWeight.
// The transient prelude also proves that quota mode is in effect.
#[test]
fn affinity_rebind_quota_rebinds_when_auth_removed_or_zero_weight() {
    for removed in [false, true] {
        let h = setup(Settings {
            routing_strategy: RoutingStrategy::Weighted,
            ..settings()
        });
        let sess = stable();
        let tried = HashSet::new();
        assert_eq!(pick(&h, &sess, false, &tried), "high");
        outcome(&h, &sess, false, "high", Some(failure(502, "overloaded")));
        assert_eq!(pick(&h, &sess, false, &tried), "mid");
        recover(&h, "high");
        assert_eq!(pick(&h, &sess, false, &tried), "high");
        {
            let mut state = h.manager.lock();
            if removed {
                // Remove from the candidate list without explicitly invalidating affinity.
                state.auths.remove("high");
            } else {
                Arc::make_mut(&mut state.auths.get_mut("high").unwrap().auth)
                    .attributes
                    .insert("weight".into(), "0".into());
            }
        }
        assert_eq!(pick(&h, &sess, false, &tried), "mid");
        assert_eq!(pick(&h, &sess, false, &tried), "mid");
        if !removed {
            Arc::make_mut(&mut h.manager.lock().auths.get_mut("high").unwrap().auth)
                .attributes
                .insert("weight".into(), "1".into());
            assert_eq!(pick(&h, &sess, false, &tried), "mid");
        }
    }
}

// TestServiceApplyConfigRuntimeSessionAffinityRebindOnChangeRecreatesSelector.
#[test]
fn affinity_rebind_policy_reload_recreates_affinity() {
    let initial = Settings {
        session_affinity_rebind_on: "any".into(),
        ..settings()
    };
    let h = setup(initial.clone());
    let sess = stable();
    let tried = HashSet::new();
    // Bind a lower priority credential so an affinity reset is observable.
    let used = HashSet::from(["high".into()]);
    assert_eq!(pick(&h, &sess, false, &used), "mid");
    h.manager.set_settings(Settings {
        session_affinity_rebind_on: String::new(),
        ..initial
    });
    assert_eq!(pick(&h, &sess, false, &tried), "mid");
    h.manager.set_settings(settings());
    assert_eq!(pick(&h, &sess, false, &tried), "high");
    // Equivalent quota spelling and disabled affinity normalize as upstream does.
    let mut cfg = crate::config::Config::default();
    cfg.routing.session_affinity = true;
    cfg.routing.session_affinity_rebind_on = " QuOtA ".into();
    assert_eq!(Settings::from(&cfg).session_affinity_rebind_on, "quota");
    use crate::manager::settings::RoutingState;
    assert_eq!(
        RoutingState::of(&settings()),
        RoutingState::of(&Settings {
            session_affinity_rebind_on: " QuOtA ".into(),
            ..settings()
        })
    );
    assert_eq!(
        RoutingState::of(&Settings::default()),
        RoutingState::of(&Settings {
            session_affinity_rebind_on: "quota".into(),
            ..Settings::default()
        })
    );
}

// TestManagerExecuteAndStreamRebindOnQuotaEndToEnd.
#[tokio::test(start_paused = true)]
async fn affinity_rebind_quota_execute_and_stream_end_to_end() {
    for disable_cooling in [false, true] {
        let h = setup(Settings {
            disable_cooling,
            ..settings()
        });
        let executor = FakeExecutor::with("codex", |call| Reply::ok(call.auth_id.clone()));
        h.executor(&executor);
        let opts = || {
            let mut opts = options();
            opts.headers = headers(&[("Session-Id", "stable")]);
            opts
        };
        let providers = vec!["codex".into()];
        assert_eq!(
            h.manager
                .execute(&providers, request(MODEL), opts())
                .await
                .unwrap()
                .payload,
            "high"
        );
        executor.set_handler(|call| match call.auth_id.as_str() {
            "high" => Reply::status(502, "502 overloaded"),
            "mid" => Reply::status(503, "503 connect error"),
            _ => Reply::ok(call.auth_id.clone()),
        });
        assert_eq!(
            h.manager
                .execute(&providers, request(MODEL), opts())
                .await
                .unwrap()
                .payload,
            "low"
        );
        executor.set_handler(|call| Reply::ok(call.auth_id.clone()));
        recover(&h, "high");
        recover(&h, "mid");
        let mut response = h
            .manager
            .execute_stream(&providers, request(MODEL), opts())
            .await
            .unwrap();
        assert_eq!(response.chunks.next().await.unwrap().unwrap(), "high");
        assert!(response.chunks.next().await.is_none());
        executor.set_handler(|call| {
            if call.auth_id == "high" {
                Reply::Err(ExecError::empty_stream())
            } else {
                Reply::ok(call.auth_id.clone())
            }
        });
        let mut response = h
            .manager
            .execute_stream(&providers, request(MODEL), opts())
            .await
            .unwrap();
        assert_eq!(response.chunks.next().await.unwrap().unwrap(), "mid");
        assert!(response.chunks.next().await.is_none());
        executor.set_handler(|call| Reply::ok(call.auth_id.clone()));
        recover(&h, "high");
        assert_eq!(
            h.manager
                .execute(&providers, request(MODEL), opts())
                .await
                .unwrap()
                .payload,
            "high"
        );
        executor.set_handler(|call| {
            if call.auth_id == "high" {
                Reply::status(429, "quota")
            } else {
                Reply::ok(call.auth_id.clone())
            }
        });
        assert_eq!(
            h.manager
                .execute(&providers, request(MODEL), opts())
                .await
                .unwrap()
                .payload,
            "mid"
        );
        executor.set_handler(|call| Reply::ok(call.auth_id.clone()));
        recover(&h, "high");
        assert_eq!(
            h.manager
                .execute(&providers, request(MODEL), opts())
                .await
                .unwrap()
                .payload,
            "mid"
        );
    }
}

// Additional coverage for conductor_selection.go's affinityCandidates boundary:
// tried credentials remain eligible for preservation, unsupported ones do not.
#[test]
fn affinity_rebind_quota_tracks_tried_after_model_support() {
    let h = setup(settings());
    let sess = stable();
    let tried = HashSet::new();
    assert_eq!(pick(&h, &sess, false, &tried), "high");
    assert_eq!(
        pick(&h, &sess, false, &HashSet::from(["high".into()])),
        "mid"
    );
    assert_eq!(pick(&h, &sess, false, &tried), "high");
    h.models.register("high", &["other-model"]);
    assert_eq!(pick(&h, &sess, false, &tried), "mid");
    h.models.register("high", &[MODEL]);
    assert_eq!(pick(&h, &sess, false, &tried), "mid");
}

// selector.go's isStateBlockedByQuotaOrAuth and shouldRebindSessionAffinityOnError.
#[test]
fn affinity_rebind_quota_classifies_permanent_and_model_blocks() {
    for (error, unavailable, retry, quota, preserve) in [
        (None, true, true, false, true),
        (None, true, false, false, false),
        (None, false, false, true, false),
        (Some(failure(502, "overloaded")), true, true, false, true),
        (Some(failure(401, "unauthorized")), true, true, false, false),
        (Some(failure(402, "payment")), true, true, false, false),
        (Some(failure(404, "missing")), true, true, false, false),
        (
            Some(failure(400, "The requested model is not supported.")),
            true,
            true,
            false,
            false,
        ),
        (
            Some(failure(400, r#"{"error":"invalid_grant"}"#)),
            true,
            true,
            false,
            false,
        ),
        (
            Some(failure(403, "<title>Just a moment...</title> cloudflare")),
            true,
            true,
            false,
            false,
        ),
    ] {
        let h = setup(settings());
        let sess = stable();
        let tried = HashSet::new();
        assert_eq!(pick(&h, &sess, false, &tried), "high");
        {
            let mut guard = h.manager.lock();
            let auth = Arc::make_mut(&mut guard.auths.get_mut("high").unwrap().auth);
            auth.model_states.insert(
                MODEL.into(),
                crate::auth::ModelState {
                    unavailable,
                    next_retry_after: retry.then(|| h.now() + chrono::TimeDelta::minutes(1)),
                    last_error: error,
                    quota: crate::auth::QuotaState {
                        exceeded: quota,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            );
        }
        assert_eq!(pick(&h, &sess, false, &tried), "mid");
        recover(&h, "high");
        assert_eq!(
            pick(&h, &sess, false, &tried),
            if preserve { "high" } else { "mid" }
        );
    }
}
