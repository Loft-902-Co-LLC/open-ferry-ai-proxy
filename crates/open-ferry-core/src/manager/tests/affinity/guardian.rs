// Ported from CLIProxyAPI sdk/cliproxy/auth/selector_guardian_affinity_test.go
// (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Guardian inheritance through real selections and the manager's candidate
//! filters. Kimi and plugin-only invalidation are not ported. Time is explicit.

use super::*;
use crate::auth::ModelState;

fn options(id: &str, parent: &str, subagent: &str) -> Session {
    session(
        &[
            ("Session_id", id),
            ("X-Codex-Parent-Thread-Id", parent),
            ("X-Openai-Subagent", subagent),
        ],
        "",
    )
}

fn guardian(id: &str, parent: &str) -> Session {
    options(id, parent, "guardian")
}

fn pool(provider: &str, affinity: Affinity) -> Sticky {
    let mut pool = Sticky::with(RoutingStrategy::FillFirst, affinity);
    for id in AB {
        pool.put(Auth {
            provider: provider.into(),
            ..cred(id)
        });
    }
    pool
}

// TestSessionAffinityGuardianParentCredential, including an ordinary child
// next to every guardian so the exception cannot leak into ordinary routing.
#[test]
fn parent_credential() {
    for case in [
        "cross_model",
        "custom_review_model",
        "openai_provider",
        "mixed_pool",
        "latest_parent_binding",
        "header_case_and_whitespace",
        "mixed_parent_real_child",
        "real_parent_mixed_child",
        "result_refresh_does_not_reorder",
        "existing_child_wins",
        "fork",
        "ordinary_subagent",
        "no_header",
        "no_parent",
        "subagents_disabled",
        "parent_not_candidate",
        "parent_disabled",
    ] {
        let real = if case == "openai_provider" {
            "openai"
        } else {
            "codex"
        };
        let parent_scope = if matches!(case, "mixed_pool" | "mixed_parent_real_child") {
            "mixed"
        } else {
            real
        };
        let child_scope = if case == "real_parent_mixed_child" {
            "mixed"
        } else if case == "mixed_parent_real_child" {
            real
        } else {
            parent_scope
        };
        let review_model = if matches!(case, "custom_review_model" | "header_case_and_whitespace") {
            "custom-review-model"
        } else {
            "codex-auto-review"
        };
        let mut pool = pool(real, Affinity::new(MINUTE, case != "subagents_disabled"));
        let parent = guardian("main-thread", "");
        let older = matches!(
            case,
            "latest_parent_binding" | "result_refresh_does_not_reorder"
        );
        if older {
            assert_eq!(
                pool.pick(parent_scope, review_model, &parent, &[AB[0]]),
                AB[0]
            );
        }
        let child = guardian("review-thread", "main-thread");
        if case == "existing_child_wins" {
            assert_eq!(
                pool.pick(child_scope, review_model, &child, &[AB[0]]),
                AB[0]
            );
        }
        assert_eq!(
            pool.pick(parent_scope, "parent-model", &parent, &[AB[1]]),
            AB[1]
        );
        if case == "result_refresh_does_not_reorder" {
            pool.now += TimeDelta::milliseconds(10);
            pool.on_result(
                &parent,
                &CallResult {
                    success: true,
                    ..failed(AB[0], review_model)
                },
                parent_scope,
            );
        }
        let child = match case {
            "header_case_and_whitespace" => session(
                &[
                    ("Session_id", "review-thread"),
                    ("X-Codex-Parent-Thread-Id", "main-thread"),
                    ("x-openai-subagent", "  GuArDiAn  "),
                ],
                "",
            ),
            "ordinary_subagent" => options("review-thread", "main-thread", "collab_spawn"),
            "no_header" => options("review-thread", "main-thread", ""),
            "no_parent" => guardian("review-thread", ""),
            "fork" => session(
                &[
                    ("Session_id", "review-thread"),
                    ("X-Codex-Parent-Thread-Id", "main-thread"),
                    ("X-Openai-Subagent", "guardian"),
                ],
                r#"{"forked_from_thread_id":"main-thread"}"#,
            ),
            _ => child,
        };
        if case == "parent_disabled" {
            let mut auth = (*pool.auths[AB[1]].auth).clone();
            auth.disabled = true;
            pool.put(auth);
        }
        let ids = if case == "parent_not_candidate" {
            &AB[..1]
        } else {
            &AB
        };
        let want = if matches!(
            case,
            "existing_child_wins"
                | "fork"
                | "ordinary_subagent"
                | "no_header"
                | "no_parent"
                | "subagents_disabled"
                | "parent_not_candidate"
                | "parent_disabled"
        ) {
            AB[0]
        } else {
            AB[1]
        };
        assert_eq!(
            pool.pick(child_scope, review_model, &child, ids),
            want,
            "{case}"
        );
        assert_eq!(
            pool.pick(
                child_scope,
                review_model,
                &options("ordinary-neighbor", "main-thread", "collab_spawn"),
                ids
            ),
            AB[0],
            "{case}"
        );
    }
}

// TestSessionAffinityGuardianParentLifecycle. Direct cache Invalidate is
// plugin-only; credential invalidation, failure and expiry use our own APIs.
#[test]
fn parent_lifecycle() {
    for action in [
        "read_only",
        "expired",
        "auth_invalidated",
        "failed",
        "evicted",
    ] {
        let affinity = if action == "evicted" {
            Affinity::with_capacity(MINUTE, true, 1)
        } else {
            Affinity::new(MINUTE, true)
        };
        let mut pool = pool("codex", affinity);
        let parent = guardian("main-thread", "");
        pool.pick("codex", "parent-model", &parent, &[AB[1]]);
        pool.now += TimeDelta::seconds(40);
        match action {
            "expired" => pool.now += TimeDelta::seconds(20),
            "auth_invalidated" => pool.affinity.invalidate_auth(AB[1]),
            "failed" => pool.on_result(&parent, &failed(AB[1], "parent-model"), "codex"),
            "evicted" => {
                pool.pick("codex", "other-model", &guardian("unrelated", ""), &[AB[0]]);
                assert_eq!(pool.bound("codex::codex:main-thread::parent-model"), None);
            }
            _ => {}
        }
        let child = guardian("review-thread", "main-thread");
        let want = if action == "read_only" { AB[1] } else { AB[0] };
        assert_eq!(
            pool.pick("codex", "review-model", &child, &AB),
            want,
            "{action}"
        );
        if action == "read_only" {
            pool.on_result(
                &child,
                &CallResult {
                    success: true,
                    ..failed(AB[1], "review-model")
                },
                "codex",
            );
            pool.on_result(&child, &failed(AB[1], "review-model"), "codex");
            assert_eq!(pool.pick("codex", "review-model", &child, &[AB[0]]), AB[0]);
            assert_eq!(
                pool.bound("codex::codex:main-thread::parent-model")
                    .as_deref(),
                Some(AB[1])
            );
            pool.now += TimeDelta::seconds(20);
            assert_eq!(
                pool.bound("codex::codex:main-thread::parent-model"),
                None,
                "guardian lookup and its outcome extended the parent TTL"
            );
            assert_eq!(
                pool.pick(
                    "codex",
                    "review-model",
                    &guardian("later-review", "main-thread"),
                    &AB
                ),
                AB[0]
            );
        }
    }
}

// TestSessionAffinityGuardianMixedProviderIsolation.
#[test]
fn mixed_provider_isolation() {
    let mut pool = pool("codex", Affinity::new(MINUTE, true));
    for id in ["auth-c", "auth-d"] {
        pool.put(Auth {
            provider: "openai".into(),
            ..cred(id)
        });
    }
    let parent = guardian("main-thread", "");
    pool.pick("mixed", "codex-parent-model", &parent, &[AB[1]]);
    pool.pick("mixed", "openai-parent-model", &parent, &["auth-d"]);
    assert_eq!(
        pool.pick(
            "mixed",
            "review-model",
            &guardian("codex-review", "main-thread"),
            &AB
        ),
        AB[1]
    );
    assert_eq!(
        pool.pick(
            "codex",
            "review-model",
            &guardian("direct-review", "main-thread"),
            &AB
        ),
        AB[1]
    );
    assert_eq!(
        pool.pick(
            "mixed",
            "review-model",
            &guardian("all-review", "main-thread"),
            &[AB[0], AB[1], "auth-c", "auth-d"]
        ),
        "auth-d"
    );
    pool.pick(
        "mixed",
        "parent-model",
        &guardian("codex-only", ""),
        &[AB[1]],
    );
    assert_eq!(
        pool.pick(
            "mixed",
            "review-model",
            &guardian("openai-review", "codex-only"),
            &["auth-c", "auth-d"]
        ),
        "auth-c"
    );
}

#[test]
fn parent_cache_hit_becomes_the_latest_selection() {
    let mut pool = pool("codex", Affinity::new(MINUTE, true));
    let parent = guardian("main-thread", "");
    pool.pick("codex", "older-model", &parent, &[AB[1]]);
    pool.pick("codex", "newer-model", &parent, &[AB[0]]);
    assert_eq!(pool.pick("codex", "older-model", &parent, &AB), AB[1]);
    assert_eq!(
        pool.pick(
            "codex",
            "review-model",
            &guardian("review", "main-thread"),
            &AB
        ),
        AB[1]
    );
}

// Pins the upstream provider-scope guard even for a deliberately broad pool.
#[test]
fn direct_route_parent_provider_stays_in_scope() {
    let mut pool = pool("codex", Affinity::new(MINUTE, true));
    pool.put(Auth {
        provider: "openai".into(),
        ..cred("auth-c")
    });
    pool.pick(
        "mixed",
        "parent-model",
        &guardian("main-thread", ""),
        &["auth-c"],
    );
    // Production direct-route policy already filters foreign credentials;
    // this deliberately broad pool pins the selector's upstream scope guard.
    assert_eq!(
        pool.pick(
            "codex",
            "review-model",
            &guardian("review", "main-thread"),
            &[AB[0], "auth-c"]
        ),
        AB[0]
    );
}

// TestSessionAffinityGuardianSchedulingProvider, for served providers only.
#[test]
fn scheduling_provider() {
    for (provider, label, attrs, scope) in [
        (" Codex ", "", vec![], "codex"),
        (
            "openai-compatibility",
            "",
            vec![("compat_name", "Review"), ("provider_key", "review")],
            "openai-compatible-review",
        ),
        (
            "openai-compatibility",
            "Review",
            vec![],
            "openai-compatible-review",
        ),
    ] {
        for (parent_scope, child_scope) in [(scope, scope), ("mixed", scope), (scope, "mixed")] {
            let mut pool = pool(provider, Affinity::new(MINUTE, true));
            for id in AB {
                let mut auth = (*pool.auths[id].auth).clone();
                auth.label = label.into();
                for (key, value) in &attrs {
                    auth.attributes.insert((*key).into(), (*value).into());
                }
                pool.put(auth);
            }
            pool.pick(
                parent_scope,
                "parent-model",
                &guardian("main-thread", ""),
                &[AB[1]],
            );
            assert_eq!(
                pool.pick(
                    child_scope,
                    "review-model",
                    &guardian("review-thread", "main-thread"),
                    &AB
                ),
                AB[1],
                "{provider} {parent_scope} {child_scope}"
            );
        }
    }
}

// TestSessionAffinityGuardianQuotaPreservedParent, strengthened to detect
// reordering against another model's newer real selection.
#[test]
fn quota_preserved_parent_does_not_record_a_selection() {
    let affinity = Affinity::for_settings(&Settings {
        session_affinity: true,
        session_affinity_rebind_on: "quota".into(),
        ..Settings::default()
    })
    .unwrap();
    let mut pool = pool("codex", affinity);
    let parent = guardian("main-thread", "");
    pool.pick("codex", "parent-model", &parent, &[AB[1]]);
    let mut auth = (*pool.auths[AB[1]].auth).clone();
    auth.model_states.insert(
        "parent-model".into(),
        ModelState {
            unavailable: true,
            next_retry_after: Some(pool.now + TimeDelta::hours(1)),
            last_error: Some(AuthError {
                http_status: 503,
                ..AuthError::default()
            }),
            ..ModelState::default()
        },
    );
    pool.put(auth);
    assert_eq!(pool.pick("codex", "parent-model", &parent, &AB), AB[0]);
    assert_eq!(
        pool.pick(
            "codex",
            "review-model",
            &guardian("first-review", "main-thread"),
            &AB
        ),
        AB[1]
    );
    pool.pick("codex", "newer-model", &parent, &[AB[0]]);
    pool.pick("codex", "parent-model", &parent, &AB);
    assert_eq!(
        pool.pick(
            "codex",
            "review-model",
            &guardian("later-review", "main-thread"),
            &AB
        ),
        AB[0]
    );
}

// Preserved failover through a conversation alias must not select the prompt key.
#[test]
fn quota_preserved_fallback_key_does_not_record_a_selection() {
    let affinity = Affinity::for_settings(&Settings {
        session_affinity: true,
        session_affinity_rebind_on: "quota".into(),
        ..Settings::default()
    })
    .unwrap();
    let mut pool = pool("codex", affinity);
    let mut fresh = (*pool.auths[AB[1]].auth).clone();
    fresh.attributes.insert("priority".into(), "1".into());
    pool.put(fresh);
    let conversation = body(r#"{"conversation":"conversation"}"#);
    assert_eq!(pool.pick("codex", "model", &conversation, &[AB[0]]), AB[0]);
    let original = (*pool.auths[AB[0]].auth).clone();
    let mut blocked = original.clone();
    blocked.model_states.insert(
        "model".into(),
        ModelState {
            unavailable: true,
            next_retry_after: Some(pool.now + TimeDelta::hours(1)),
            last_error: Some(AuthError {
                http_status: 503,
                ..AuthError::default()
            }),
            ..ModelState::default()
        },
    );
    pool.put(blocked);
    let prompt = body(r#"{"prompt_cache_key":"prompt","conversation":"conversation"}"#);
    assert_eq!(pool.bound("codex::pck:prompt::model"), None);
    assert_eq!(pool.pick("codex", "model", &prompt, &AB), AB[1]);
    assert_eq!(
        pool.bound("codex::pck:prompt::model").as_deref(),
        Some(AB[0])
    );
    assert_eq!(
        pool.bound("codex::conv:conversation::model").as_deref(),
        Some(AB[0])
    );
    pool.put(original);
    let child = session(
        &[("X-Openai-Subagent", "guardian")],
        r#"{"prompt_cache_key":"review","parent_session_id":"prompt"}"#,
    );
    assert_eq!(child.fallback(), "pck:prompt");
    assert_eq!(pool.pick("codex", "review-model", &child, &AB), AB[1]);
}

// TestSessionAffinityQuotaPreservedBindingAddsConversationAlias.
#[test]
fn quota_preserved_binding_adds_conversation_alias() {
    let affinity = Affinity::for_settings(&Settings {
        session_affinity: true,
        session_affinity_rebind_on: "quota".into(),
        ..Settings::default()
    })
    .unwrap();
    let mut pool = pool("codex", affinity);
    pool.pick(
        "codex",
        "model",
        &body(r#"{"prompt_cache_key":"prompt"}"#),
        &[AB[1]],
    );
    let original = (*pool.auths[AB[1]].auth).clone();
    let mut blocked = original.clone();
    blocked.model_states.insert(
        "model".into(),
        ModelState {
            unavailable: true,
            next_retry_after: Some(pool.now + TimeDelta::hours(1)),
            last_error: Some(AuthError {
                http_status: 503,
                ..AuthError::default()
            }),
            ..ModelState::default()
        },
    );
    pool.put(blocked);
    assert_eq!(
        pool.pick(
            "codex",
            "model",
            &body(r#"{"prompt_cache_key":"prompt","conversation":"conversation"}"#),
            &AB
        ),
        AB[0]
    );
    pool.put(original);
    assert_eq!(
        pool.pick(
            "codex",
            "model",
            &body(r#"{"conversation":"conversation"}"#),
            &AB
        ),
        AB[1]
    );
}

// TestSessionAffinityGuardianCandidateEligibility, with additional disabled,
// unsupported, unavailable and already-tried cases through route filtering.
#[test]
fn candidate_eligibility() {
    for case in [
        "lower_priority",
        "model_cooldown",
        "zero_weight",
        "disabled",
        "unsupported",
        "unavailable",
        "already_tried",
    ] {
        let mut pool = pool("codex", Affinity::new(MINUTE, true));
        if case == "zero_weight" {
            pool.strategy = RoutingStrategy::Weighted;
        }
        let parent = guardian("main-thread", "");
        pool.pick("mixed", "parent-model", &parent, &[AB[1]]);
        let mut auth = (*pool.auths[AB[1]].auth).clone();
        match case {
            "lower_priority" => {
                auth.attributes.insert("priority".into(), "-1".into());
            }
            "model_cooldown" => {
                auth.model_states.insert(
                    "review-model".into(),
                    ModelState {
                        unavailable: true,
                        next_retry_after: Some(pool.now + TimeDelta::hours(1)),
                        ..ModelState::default()
                    },
                );
            }
            "zero_weight" => {
                auth.attributes.insert("weight".into(), "0".into());
            }
            "disabled" => auth.disabled = true,
            "unavailable" => {
                auth.unavailable = true;
                auth.next_retry_after = Some(pool.now + TimeDelta::hours(1));
            }
            _ => {}
        }
        pool.put(auth);
        pool.models
            .register(AB[0], &["parent-model", "review-model"]);
        pool.models.register(
            AB[1],
            if case == "unsupported" {
                &["parent-model"]
            } else {
                &["parent-model", "review-model"]
            },
        );
        pool.executors
            .insert("codex".into(), FakeExecutor::new("codex"));
        let tried = if case == "already_tried" {
            HashSet::from([AB[1].into()])
        } else {
            HashSet::new()
        };
        let selection = Selection {
            auths: &pool.auths,
            executors: &pool.executors,
            models: &pool.models,
            resolver: Resolver {
                settings: &pool.settings,
                oauth: &pool.oauth,
            },
            strategy: pool.strategy,
            now: pool.now,
        };
        let child = guardian("review-thread", "main-thread");
        let picked = selection
            .pick_next_mixed_sticky(
                &mut pool.state,
                &mut pool.affinity,
                &["codex".into()],
                &PickArgs {
                    model: "review-model",
                    pinned: "",
                    downstream_websocket: false,
                    tried: &tried,
                    eligibility: Default::default(),
                },
                Some(&child),
            )
            .unwrap();
        assert_eq!(
            picked.auth.id,
            if case == "lower_priority" {
                AB[1]
            } else {
                AB[0]
            },
            "{case}"
        );
    }
}
