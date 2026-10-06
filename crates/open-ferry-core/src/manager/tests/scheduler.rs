// Ported from CLIProxyAPI sdk/cliproxy/auth/scheduler_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Which credential a pick takes: priority tiers, round robin, fill first
//! and smooth weighted round robin, the WebSocket subset, mixed providers,
//! and rotation kept across cooldowns, recoveries and weight changes.
//!
//! Deviations from upstream:
//! - Upstream picks through its scheduler index (`pickSingle`,
//!   `pickMixed`); here every pick is one `Selection::pick_next_mixed` over
//!   the manager's credentials, which needs an executor for the provider, so
//!   each test registers a fake one. Credentials go in through `register`
//!   and change through `update` where upstream calls `rebuild` and
//!   `upsertAuth`.
//! - ReadyViewRoundRobinPreservesSuccessorAcrossRebuild: upstream snapshots
//!   and restores a ready view's cursor by hand; here the credentials cool
//!   down and recover through `update`, and the next pick shows where the
//!   rotation resumed. The retry exclusion case uses the tried set.
//! - ManagerLegacyWeightedRoundRobinKeepsIndependentAliasPrefixedModelState:
//!   upstream forces the legacy path with a plugin scheduler that declines;
//!   plugin schedulers aren't ported, so the test calls the legacy pick
//!   directly.
//! - Manager_InitializesSchedulerForBuiltInSelector: upstream checks the
//!   scheduler's strategy field; here the strategy is a setting, so the test
//!   checks the setting and that picks follow it after `set_settings`.
//! - Manager_SchedulerSharesThinkingSuffixCooldownAndRegistryState: the
//!   registry's model count is counted from the projections the manager
//!   published to the fake registry.
//! - The Codex Alpha Search credential policy tests are in
//!   `credential_policy`.
//! - Manager_PickNextMixed_DisallowFreeAuthSkipsCodexFreePlan: the flag is
//!   the pick's eligibility, which a call takes from its metadata.
//! - Dropped: the plugin scheduler tests (not ported), the Home dispatcher
//!   tests (not ported), the auth kind tests (required auth kinds aren't
//!   ported, see manager/mod.rs), and CustomSelector_FallsBackToLegacyPath
//!   (the port has no pluggable selector, only the routing strategy
//!   setting).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::TimeDelta;
use serde_json::json;

use super::support::*;
use crate::auth::{Auth, AuthError, ModelState, Status};
use crate::exec::{Dispatcher, ExecError};
use crate::manager::models::Resolver;
use crate::manager::policy::Eligibility;
use crate::manager::retry::{RetryQuery, should_retry_after_error};
use crate::manager::select::{ClientModels, PickArgs, Selection, successor_index};
use crate::manager::{CallResult, RoutingStrategy, Settings};

/// A credential with `attributes`.
fn cred(id: &str, provider: &str, attributes: &[(&str, &str)]) -> Auth {
    let mut auth = auth(id, provider);
    for (key, value) in attributes {
        auth.attributes
            .insert((*key).to_owned(), (*value).to_owned());
    }
    auth
}

/// A manager with `strategy` and a fake executor for each provider.
fn harness(strategy: RoutingStrategy, executors: &[&str]) -> Harness {
    let h = Harness::new(Settings {
        routing_strategy: strategy,
        ..Settings::default()
    });
    for provider in executors {
        h.executor(&FakeExecutor::new(provider));
    }
    h
}

/// Builds the selection a call would use and runs `f` with it.
fn with_selection<T>(
    h: &Harness,
    f: impl FnOnce(&Selection<'_>, &mut crate::manager::select::SelectorState) -> T,
) -> T {
    let now = h.now();
    let mut guard = h.manager.lock();
    let state = &mut *guard;
    let settings = state.settings.clone();
    let oauth = state.oauth.clone();
    let selection = Selection {
        auths: &state.auths,
        executors: &state.executors,
        models: h.manager.models(),
        resolver: Resolver {
            settings: &settings,
            oauth: &oauth,
        },
        strategy: settings.routing_strategy,
        now,
    };
    f(&selection, &mut state.selector)
}

/// One pick over `providers`: the credential and its provider.
fn try_pick(
    h: &Harness,
    providers: &[&str],
    model: &str,
    websocket: bool,
    tried: &HashSet<String>,
) -> Result<(String, String), ExecError> {
    let providers: Vec<String> = providers.iter().map(|p| (*p).to_owned()).collect();
    let args = PickArgs {
        model,
        pinned: "",
        downstream_websocket: websocket,
        eligibility: Default::default(),
        tried,
    };
    with_selection(h, |selection, state| {
        selection
            .pick_next_mixed(state, &providers, &args)
            .map(|picked| (picked.auth.id.clone(), picked.provider))
    })
}

/// The credential one pick for `provider` and `model` takes.
fn pick(h: &Harness, provider: &str, model: &str) -> String {
    match try_pick(h, &[provider], model, false, &HashSet::new()) {
        Ok((id, _)) => id,
        Err(err) => panic!("pick {provider} {model:?}: {err}"),
    }
}

/// The credential one pick for a client on a WebSocket takes.
fn pick_ws(h: &Harness, provider: &str) -> String {
    match try_pick(h, &[provider], "", true, &HashSet::new()) {
        Ok((id, _)) => id,
        Err(err) => panic!("websocket pick {provider}: {err}"),
    }
}

/// The credential and provider one pick over `providers` takes.
fn pick_mixed(
    h: &Harness,
    providers: &[&str],
    model: &str,
    tried: &HashSet<String>,
) -> (String, String) {
    match try_pick(h, providers, model, false, tried) {
        Ok(picked) => picked,
        Err(err) => panic!("pick {providers:?} {model:?}: {err}"),
    }
}

fn tally(ids: impl IntoIterator<Item = String>) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for id in ids {
        *counts.entry(id).or_insert(0) += 1;
    }
    counts
}

fn want(pairs: &[(&str, usize)]) -> HashMap<String, usize> {
    pairs.iter().map(|(id, n)| ((*id).to_owned(), *n)).collect()
}

/// Replaces credential `id` with `change` applied.
fn update(h: &Harness, id: &str, change: impl FnOnce(&mut Auth)) {
    let mut auth = (*h.get(id)).clone();
    change(&mut auth);
    h.manager
        .update(auth)
        .expect("update")
        .expect("credential registered");
}

/// Makes credential `id` unavailable for an hour.
fn cool(h: &Harness, id: &str) {
    let until = h.now() + TimeDelta::hours(1);
    update(h, id, |auth| {
        auth.unavailable = true;
        auth.next_retry_after = Some(until);
    });
}

fn failure(id: &str, model: &str, message: &str) -> CallResult {
    CallResult {
        auth_id: id.into(),
        provider: "gemini".into(),
        model: model.into(),
        success: false,
        error: Some(AuthError {
            http_status: 429,
            message: message.into(),
            ..AuthError::default()
        }),
        ..CallResult::default()
    }
}

fn success(id: &str, model: &str) -> CallResult {
    CallResult {
        auth_id: id.into(),
        provider: "gemini".into(),
        model: model.into(),
        success: true,
        ..CallResult::default()
    }
}

/// Upstream's registry `GetModelCount`: the clients serving `model` that
/// the manager hasn't published as suspended or out of quota.
fn model_count(h: &Harness, clients: &[&str], model: &str) -> usize {
    clients
        .iter()
        .filter(|id| h.models.models_for_client(id).iter().any(|m| m == model))
        .filter(|id| {
            !h.models
                .projection(id, model)
                .is_some_and(|p| p.suspended || p.quota_exceeded)
        })
        .count()
}

#[tokio::test(start_paused = true)]
async fn scheduler_pick_round_robin_highest_priority() {
    let h = harness(RoutingStrategy::RoundRobin, &["gemini"]);
    h.add(cred("low", "gemini", &[("priority", "0")]), &[]);
    h.add(cred("high-b", "gemini", &[("priority", "10")]), &[]);
    h.add(cred("high-a", "gemini", &[("priority", "10")]), &[]);

    for (index, want_id) in ["high-a", "high-b", "high-a"].iter().enumerate() {
        assert_eq!(pick(&h, "gemini", ""), *want_id, "pick #{index}");
    }
}

#[tokio::test(start_paused = true)]
async fn scheduler_pick_weighted_round_robin() {
    let h = harness(RoutingStrategy::Weighted, &["gemini"]);
    h.add(cred("a", "gemini", &[("weight", "5")]), &[]);
    h.add(cred("b", "gemini", &[("weight", "3")]), &[]);
    h.add(cred("c", "gemini", &[("weight", "2")]), &[]);

    let counts = tally((0..100).map(|_| pick(&h, "gemini", "")));
    assert_eq!(counts, want(&[("a", 50), ("b", 30), ("c", 20)]));
}

#[tokio::test(start_paused = true)]
async fn manager_load_weighted_round_robin_uses_persisted_metadata_weight() {
    let h = Harness::with_store(Settings {
        routing_strategy: RoutingStrategy::Weighted,
        ..Settings::default()
    });
    h.executor(&FakeExecutor::new("gemini"));
    h.store
        .put(auth_with_metadata("a", "gemini", json!({"weight": 5.0})));
    h.store
        .put(auth_with_metadata("b", "gemini", json!({"weight": 1.0})));
    h.manager.load().expect("load");

    let counts = tally((0..60).map(|_| pick(&h, "gemini", "")));
    assert_eq!(counts, want(&[("a", 50), ("b", 10)]));
}

#[tokio::test(start_paused = true)]
async fn scheduler_pick_weighted_round_robin_resets_credits_when_weights_change() {
    let h = harness(RoutingStrategy::Weighted, &["gemini"]);
    h.add(cred("a", "gemini", &[("weight", "1000000")]), &[]);
    h.add(cred("b", "gemini", &[("weight", "1")]), &[]);
    for _ in 0..1000 {
        pick(&h, "gemini", "");
    }

    update(&h, "a", |auth| {
        auth.attributes.insert("weight".into(), "1".into());
    });
    let counts = tally((0..20).map(|_| pick(&h, "gemini", "")));
    assert_eq!(counts, want(&[("a", 10), ("b", 10)]));
}

#[tokio::test(start_paused = true)]
async fn scheduler_pick_weighted_websocket_resets_credits_when_weights_change() {
    let h = harness(RoutingStrategy::Weighted, &["codex"]);
    h.add(
        cred(
            "a",
            "codex",
            &[("weight", "1000000"), ("websockets", "true")],
        ),
        &[],
    );
    h.add(
        cred("b", "codex", &[("weight", "1"), ("websockets", "true")]),
        &[],
    );
    for _ in 0..1000 {
        pick_ws(&h, "codex");
    }

    update(&h, "a", |auth| {
        auth.attributes.insert("weight".into(), "1".into());
    });
    let counts = tally((0..20).map(|_| pick_ws(&h, "codex")));
    assert_eq!(counts, want(&[("a", 10), ("b", 10)]));
}

#[tokio::test(start_paused = true)]
async fn manager_legacy_weighted_round_robin_keeps_independent_alias_prefixed_model_state() {
    let h = harness(RoutingStrategy::Weighted, &["gemini"]);
    h.add(cred("a-heavy", "gemini", &[("weight", "3")]), &[]);
    h.add(cred("a-light", "gemini", &[("weight", "1")]), &[]);
    h.add(cred("b-light", "gemini", &[("weight", "1")]), &[]);
    h.add(cred("b-heavy", "gemini", &[("weight", "3")]), &[]);
    h.models.register("a-heavy", &["team-a/shared"]);
    h.models.register("a-light", &["team-a/shared"]);
    h.models.register("b-light", &["team-b/shared"]);
    h.models.register("b-heavy", &["team-b/shared"]);

    let providers = providers(&["gemini"]);
    let tried = HashSet::new();
    let mut picks = Vec::new();
    for index in 0..40 {
        for model in ["team-a/shared", "team-b/shared"] {
            let args = PickArgs {
                model,
                pinned: "",
                downstream_websocket: false,
                eligibility: Default::default(),
                tried: &tried,
            };
            let picked = with_selection(&h, |selection, state| {
                selection.pick_next_mixed_legacy(state, &providers, &args)
            });
            match picked {
                Ok(picked) => picks.push(picked.auth.id.clone()),
                Err(err) => panic!("legacy pick {model:?} #{index}: {err}"),
            }
        }
    }
    assert_eq!(
        tally(picks),
        want(&[
            ("a-heavy", 30),
            ("a-light", 10),
            ("b-light", 10),
            ("b-heavy", 30)
        ])
    );
}

#[tokio::test(start_paused = true)]
async fn scheduler_pick_weighted_round_robin_skips_non_positive_weight_priority_tier() {
    let h = harness(RoutingStrategy::Weighted, &["gemini"]);
    h.add(
        cred("excluded", "gemini", &[("priority", "10"), ("weight", "0")]),
        &[],
    );
    h.add(
        cred("available", "gemini", &[("priority", "0"), ("weight", "1")]),
        &[],
    );

    assert_eq!(pick(&h, "gemini", ""), "available");
}

#[tokio::test(start_paused = true)]
async fn scheduler_pick_fill_first_sticks_to_first_ready() {
    let h = harness(RoutingStrategy::FillFirst, &["gemini"]);
    h.add(auth("b", "gemini"), &[]);
    h.add(auth("a", "gemini"), &[]);
    h.add(auth("c", "gemini"), &[]);

    for index in 0..3 {
        assert_eq!(pick(&h, "gemini", ""), "a", "pick #{index}");
    }
}

#[tokio::test(start_paused = true)]
async fn scheduler_pick_promotes_expired_cooldown_before_pick() {
    let model = "gemini-2.5-pro";
    let h = harness(RoutingStrategy::RoundRobin, &["gemini"]);
    let mut cooling = auth("cooldown-expired", "gemini");
    cooling.model_states.insert(
        model.into(),
        ModelState {
            status: Status::Error,
            unavailable: true,
            next_retry_after: Some(h.now() - TimeDelta::seconds(1)),
            ..ModelState::default()
        },
    );
    h.add(cooling, &[model]);

    assert_eq!(pick(&h, "gemini", model), "cooldown-expired");
}

#[tokio::test(start_paused = true)]
async fn scheduler_pick_codex_websocket_prefers_websocket_enabled_subset() {
    let h = harness(RoutingStrategy::RoundRobin, &["codex"]);
    h.add(auth("codex-http", "codex"), &[]);
    h.add(cred("codex-ws-a", "codex", &[("websockets", "true")]), &[]);
    h.add(cred("codex-ws-b", "codex", &[("websockets", "true")]), &[]);

    for (index, want_id) in ["codex-ws-a", "codex-ws-b", "codex-ws-a"]
        .iter()
        .enumerate()
    {
        assert_eq!(pick_ws(&h, "codex"), *want_id, "pick #{index}");
    }
}

#[tokio::test(start_paused = true)]
async fn scheduler_pick_xai_websocket_prefers_websocket_enabled_subset() {
    let h = harness(RoutingStrategy::RoundRobin, &["xai"]);
    h.add(auth("xai-http", "xai"), &[]);
    h.add(cred("xai-ws-a", "xai", &[("websockets", "true")]), &[]);
    h.add(cred("xai-ws-b", "xai", &[("websockets", "true")]), &[]);

    for (index, want_id) in ["xai-ws-a", "xai-ws-b", "xai-ws-a"].iter().enumerate() {
        assert_eq!(pick_ws(&h, "xai"), *want_id, "pick #{index}");
    }
}

#[tokio::test(start_paused = true)]
async fn scheduler_pick_codex_websocket_prefers_websocket_enabled_across_priorities() {
    let h = harness(RoutingStrategy::RoundRobin, &["codex"]);
    h.add(cred("codex-http", "codex", &[("priority", "10")]), &[]);
    h.add(
        cred(
            "codex-ws-a",
            "codex",
            &[("priority", "0"), ("websockets", "true")],
        ),
        &[],
    );
    h.add(
        cred(
            "codex-ws-b",
            "codex",
            &[("priority", "0"), ("websockets", "true")],
        ),
        &[],
    );

    for (index, want_id) in ["codex-ws-a", "codex-ws-b", "codex-ws-a"]
        .iter()
        .enumerate()
    {
        assert_eq!(pick_ws(&h, "codex"), *want_id, "pick #{index}");
    }
}

/// gemini-a, gemini-b and claude-a picked over gemini and claude: each
/// provider gets turns in proportion to its ready credentials.
fn assert_provider_rotation(h: &Harness) {
    let want_providers = ["gemini", "gemini", "claude", "gemini"];
    let want_ids = ["gemini-a", "gemini-b", "claude-a", "gemini-a"];
    for index in 0..want_ids.len() {
        let (id, provider) = pick_mixed(h, &["gemini", "claude"], "", &HashSet::new());
        assert_eq!(provider, want_providers[index], "pick #{index} provider");
        assert_eq!(id, want_ids[index], "pick #{index} auth");
    }
}

#[tokio::test(start_paused = true)]
async fn scheduler_pick_mixed_providers_uses_weighted_provider_rotation_over_ready_candidates() {
    let h = harness(RoutingStrategy::RoundRobin, &["gemini", "claude"]);
    h.add(auth("gemini-a", "gemini"), &[]);
    h.add(auth("gemini-b", "gemini"), &[]);
    h.add(auth("claude-a", "claude"), &[]);

    assert_provider_rotation(&h);
}

#[tokio::test(start_paused = true)]
async fn scheduler_pick_mixed_providers_weighted_round_robin() {
    let h = harness(RoutingStrategy::Weighted, &["gemini", "claude"]);
    h.add(cred("gemini-a", "gemini", &[("weight", "5")]), &[]);
    h.add(cred("claude-b", "claude", &[("weight", "3")]), &[]);
    h.add(cred("claude-c", "claude", &[("weight", "2")]), &[]);

    let mut ids = Vec::new();
    for index in 0..100 {
        let (id, provider) = pick_mixed(&h, &["gemini", "claude"], "", &HashSet::new());
        assert!(!provider.is_empty(), "pick #{index} has no provider");
        ids.push(id);
    }
    assert_eq!(
        tally(ids),
        want(&[("gemini-a", 50), ("claude-b", 30), ("claude-c", 20)])
    );
}

#[tokio::test(start_paused = true)]
async fn scheduler_pick_mixed_providers_resets_credits_when_weights_change() {
    let h = harness(RoutingStrategy::Weighted, &["gemini", "claude"]);
    h.add(cred("gemini-a", "gemini", &[("weight", "1000000")]), &[]);
    h.add(cred("claude-b", "claude", &[("weight", "1")]), &[]);
    let providers = ["gemini", "claude"];
    for _ in 0..1000 {
        pick_mixed(&h, &providers, "", &HashSet::new());
    }

    update(&h, "gemini-a", |auth| {
        auth.attributes.insert("weight".into(), "1".into());
    });
    let counts = tally((0..20).map(|_| pick_mixed(&h, &providers, "", &HashSet::new()).0));
    assert_eq!(counts, want(&[("gemini-a", 10), ("claude-b", 10)]));
}

#[tokio::test(start_paused = true)]
async fn scheduler_pick_mixed_retry_tried_filter_preserves_smooth_weighted_distribution() {
    let providers = ["provider-a", "provider-b", "provider-c", "provider-d"];
    let h = harness(RoutingStrategy::Weighted, &providers);
    h.add(auth("auth-a", "provider-a"), &[]);
    h.add(auth("auth-b", "provider-b"), &[]);
    h.add(auth("auth-c", "provider-c"), &[]);
    h.add(auth("auth-d", "provider-d"), &[]);

    // auth-a failed and is in the tried set: the retries rotate evenly over
    // the rest, without leaning to auth-b for coming first.
    let tried: HashSet<String> = ["auth-a".to_owned()].into();
    let counts = tally((0..30).map(|_| pick_mixed(&h, &providers, "", &tried).0));
    for id in ["auth-b", "auth-c", "auth-d"] {
        assert_eq!(counts.get(id), Some(&10), "{id}: {counts:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn ready_view_round_robin_preserves_successor_across_rebuild() {
    fn setup(ids: &[&str]) -> Harness {
        let h = harness(RoutingStrategy::RoundRobin, &["gemini"]);
        for id in ids {
            h.add(auth(id, "gemini"), &[]);
        }
        h
    }

    // a cooling resumes at b
    let h = setup(&["A", "B", "C"]);
    assert_eq!(pick(&h, "gemini", ""), "A", "first pick");
    cool(&h, "A");
    assert_eq!(pick(&h, "gemini", ""), "B", "pick after A cooldown");

    // b cooling resumes at c
    let h = setup(&["A", "B", "C"]);
    assert_eq!(pick(&h, "gemini", ""), "A", "first pick");
    assert_eq!(pick(&h, "gemini", ""), "B", "second pick");
    cool(&h, "B");
    assert_eq!(pick(&h, "gemini", ""), "C", "pick after B cooldown");

    // c cooling wraps to a
    let h = setup(&["A", "B", "C"]);
    for want_id in ["A", "B", "C"] {
        assert_eq!(pick(&h, "gemini", ""), want_id);
    }
    cool(&h, "C");
    assert_eq!(pick(&h, "gemini", ""), "A", "pick after C cooldown");

    // recovery preserves successor
    let h = setup(&["A", "B", "C"]);
    cool(&h, "A");
    assert_eq!(pick(&h, "gemini", ""), "B", "first pick");
    update(&h, "A", |auth| {
        auth.unavailable = false;
        auth.next_retry_after = None;
    });
    assert_eq!(pick(&h, "gemini", ""), "C", "pick after A recovery");

    // retry exclusion resumes without rebuild
    let h = setup(&["A", "B", "C"]);
    assert_eq!(pick(&h, "gemini", ""), "A", "first pick");
    let tried: HashSet<String> = ["B".to_owned()].into();
    assert_eq!(
        pick_mixed(&h, &["gemini"], "", &tried).0,
        "C",
        "pick after excluding B"
    );

    // multiple cooldown skips to first surviving successor
    let h = setup(&["A", "B", "C", "D"]);
    assert_eq!(pick(&h, "gemini", ""), "A", "first pick");
    cool(&h, "A");
    cool(&h, "B");
    assert_eq!(pick(&h, "gemini", ""), "C", "pick after A and B cooldown");
}

#[tokio::test(start_paused = true)]
async fn scheduled_successor_index_wraps_and_skips_filtered_candidates() {
    let entries = ["aaa", "ccc", "eee"];
    let cases = [
        ("no previous pick starts at head", "", 0),
        ("resumes after previous pick", "aaa", 1),
        ("resumes after filtered-out pick", "bbb", 1),
        ("wraps at the end of the ring", "eee", 0),
        ("wraps for removed trailing pick", "zzz", 0),
    ];
    for (name, last_id, want_index) in cases {
        assert_eq!(successor_index(&entries, last_id), want_index, "{name}");
    }
    assert_eq!(successor_index(&[], "aaa"), 0);
}

#[tokio::test(start_paused = true)]
async fn manager_round_robin_preserves_successor_across_cooldown() {
    let model = "test-successor-model";
    let h = harness(RoutingStrategy::RoundRobin, &["gemini"]);
    for id in ["successor-auth-a", "successor-auth-b", "successor-auth-c"] {
        h.add(auth(id, "gemini"), &[model]);
    }

    assert_eq!(pick(&h, "gemini", model), "successor-auth-a", "pick #1");
    h.manager
        .mark_result(&failure("successor-auth-a", model, "rate limit"));
    assert_eq!(
        pick(&h, "gemini", model),
        "successor-auth-b",
        "pick #2 after successor-auth-a cooldown"
    );
    h.manager
        .mark_result(&failure("successor-auth-b", model, "rate limit"));
    assert_eq!(
        pick(&h, "gemini", model),
        "successor-auth-c",
        "pick #3 after successor-auth-b cooldown"
    );
}

/// Not an upstream test: a cooldown that starts and ends between two picks
/// still drops the cooled credential's weighted credit, as upstream's
/// rebuild on each change does. The sequences are upstream's, from running
/// the same steps against it.
#[tokio::test(start_paused = true)]
async fn weighted_credit_drops_with_a_cooldown_between_picks() {
    fn overloaded(id: &str, model: &str) -> CallResult {
        CallResult {
            error: Some(AuthError {
                http_status: 503,
                message: "overloaded".into(),
                ..AuthError::default()
            }),
            ..failure(id, model, "")
        }
    }
    fn picks(h: &Harness, model: &str, n: usize) -> Vec<String> {
        (0..n).map(|_| pick(h, "gemini", model)).collect()
    }

    // A 503, then a success.
    let model = "weighted-transition-1";
    let h = harness(RoutingStrategy::Weighted, &["gemini"]);
    h.add(cred("a", "gemini", &[("weight", "1")]), &[model]);
    h.add(cred("b", "gemini", &[("weight", "3")]), &[model]);
    assert_eq!(picks(&h, model, 1), ["b"]);
    h.manager.mark_result(&overloaded("b", model));
    h.manager.mark_result(&success("b", model));
    assert_eq!(
        picks(&h, model, 8),
        ["b", "a", "b", "b", "b", "a", "b", "b"]
    );

    // A cooldown and a recovery through update, with a pick between.
    let model = "weighted-transition-2";
    let h = harness(RoutingStrategy::Weighted, &["gemini"]);
    h.add(cred("c", "gemini", &[("weight", "1")]), &[model]);
    h.add(cred("d", "gemini", &[("weight", "3")]), &[model]);
    assert_eq!(picks(&h, model, 1), ["d"]);
    cool(&h, "d");
    assert_eq!(picks(&h, model, 1), ["c"]);
    update(&h, "d", |auth| {
        auth.unavailable = false;
        auth.next_retry_after = None;
    });
    assert_eq!(
        picks(&h, model, 8),
        ["d", "c", "d", "d", "d", "c", "d", "d"]
    );

    // Three credentials; the middle one fails and recovers twice.
    let model = "weighted-transition-3";
    let h = harness(RoutingStrategy::Weighted, &["gemini"]);
    h.add(cred("e", "gemini", &[("weight", "5")]), &[model]);
    h.add(cred("f", "gemini", &[("weight", "3")]), &[model]);
    h.add(cred("g", "gemini", &[("weight", "2")]), &[model]);
    let mut seq = picks(&h, model, 3);
    for _ in 0..2 {
        h.manager.mark_result(&overloaded("f", model));
        h.manager.mark_result(&success("f", model));
        seq.extend(picks(&h, model, 3));
    }
    assert_eq!(seq, ["e", "f", "g", "e", "f", "e", "e", "f", "g"]);
}

#[tokio::test(start_paused = true)]
async fn scheduler_pick_round_robin_preserves_websocket_successor_across_cooldown() {
    let h = harness(RoutingStrategy::RoundRobin, &["codex"]);
    h.add(auth("codex-http", "codex"), &[]);
    h.add(cred("codex-ws-a", "codex", &[("websockets", "true")]), &[]);
    h.add(cred("codex-ws-b", "codex", &[("websockets", "true")]), &[]);
    h.add(cred("codex-ws-c", "codex", &[("websockets", "true")]), &[]);

    assert_eq!(pick_ws(&h, "codex"), "codex-ws-a", "first pick");
    cool(&h, "codex-ws-a");
    assert_eq!(
        pick_ws(&h, "codex"),
        "codex-ws-b",
        "pick after ws-a cooldown"
    );
}

#[tokio::test(start_paused = true)]
async fn scheduler_pick_mixed_providers_prefers_highest_priority_tier() {
    let model = "gpt-default";
    let providers = ["provider-low", "provider-high-a", "provider-high-b"];
    let h = harness(RoutingStrategy::RoundRobin, &providers);
    h.add(cred("low", "provider-low", &[("priority", "4")]), &[model]);
    h.add(
        cred("high-a", "provider-high-a", &[("priority", "7")]),
        &[model],
    );
    h.add(
        cred("high-b", "provider-high-b", &[("priority", "7")]),
        &[model],
    );

    let want_providers = [
        "provider-high-a",
        "provider-high-b",
        "provider-high-a",
        "provider-high-b",
    ];
    let want_ids = ["high-a", "high-b", "high-a", "high-b"];
    for index in 0..want_ids.len() {
        let (id, provider) = pick_mixed(&h, &providers, model, &HashSet::new());
        assert_eq!(provider, want_providers[index], "pick #{index} provider");
        assert_eq!(id, want_ids[index], "pick #{index} auth");
    }
}

#[tokio::test(start_paused = true)]
async fn manager_pick_next_mixed_uses_weighted_provider_rotation_before_credential_rotation() {
    let h = harness(RoutingStrategy::RoundRobin, &["gemini", "claude"]);
    h.add(auth("gemini-a", "gemini"), &[]);
    h.add(auth("gemini-b", "gemini"), &[]);
    h.add(auth("claude-a", "claude"), &[]);

    assert_provider_rotation(&h);
}

/// The free-plan rule's eligibility.
const NO_FREE: Eligibility = Eligibility {
    disallow_free_auth: true,
};

/// One pick over `providers` for `model` with `eligibility`, on the
/// scheduler path or the legacy one: the credential and its provider.
fn pick_eligible(
    h: &Harness,
    providers: &[&str],
    model: &str,
    eligibility: Eligibility,
    legacy: bool,
) -> Result<(String, String), ExecError> {
    let providers: Vec<String> = providers.iter().map(|p| (*p).to_owned()).collect();
    let tried = HashSet::new();
    let args = PickArgs {
        model,
        pinned: "",
        downstream_websocket: false,
        tried: &tried,
        eligibility,
    };
    with_selection(h, |selection, state| {
        let picked = if legacy {
            selection.pick_next_mixed_legacy(state, &providers, &args)
        } else {
            selection.pick_next_mixed(state, &providers, &args)
        };
        picked.map(|picked| (picked.auth.id.clone(), picked.provider))
    })
}

/// `TestManager_PickNextMixed_DisallowFreeAuthSkipsCodexFreePlan`.
#[tokio::test(start_paused = true)]
async fn manager_pick_next_mixed_disallow_free_auth_skips_codex_free_plan() {
    let model = "gpt-5.4-mini";
    let h = harness(RoutingStrategy::RoundRobin, &["codex"]);
    h.add(
        cred("codex-a-free", "codex", &[("plan_type", "free")]),
        &[model],
    );
    h.add(
        cred("codex-b-plus", "codex", &[("plan_type", "plus")]),
        &[model],
    );

    let (id, provider) =
        pick_eligible(&h, &["codex"], model, NO_FREE, false).expect("pick_next_mixed");
    assert_eq!(provider, "codex");
    assert_eq!(id, "codex-b-plus");
}

/// Not upstream's: the rule holds on the legacy path and for every pick,
/// matches the plan and provider as upstream's `EqualFold` does, leaves
/// other providers' free plans alone, and finds nothing when only free
/// Codex credentials serve the model; a call that allows them picks them.
#[tokio::test(start_paused = true)]
async fn disallow_free_auth_holds_on_every_path() {
    let model = "gpt-image-2";
    let h = harness(RoutingStrategy::RoundRobin, &["codex", "gemini"]);
    h.add(
        cred("a-free", "Codex", &[("plan_type", " FREE ")]),
        &[model],
    );
    h.add(cred("b-plus", "codex", &[("plan_type", "plus")]), &[model]);
    h.add(cred("c-none", "codex", &[]), &[model]);
    h.add(
        cred("d-gemini", "gemini", &[("plan_type", "free")]),
        &[model],
    );
    for legacy in [false, true] {
        let picks: HashSet<String> = (0..6)
            .map(|_| {
                pick_eligible(&h, &["codex", "gemini"], model, NO_FREE, legacy)
                    .expect("pick")
                    .0
            })
            .collect();
        let want: HashSet<String> = ["b-plus", "c-none", "d-gemini"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(picks, want, "legacy {legacy}");
    }

    let h = harness(RoutingStrategy::FillFirst, &["codex"]);
    h.add(
        cred("only-free", "codex", &[("plan_type", "free")]),
        &[model],
    );
    for legacy in [false, true] {
        assert!(
            pick_eligible(&h, &["codex"], model, NO_FREE, legacy).is_err(),
            "legacy {legacy}"
        );
        assert_eq!(
            pick_eligible(&h, &["codex"], model, Eligibility::default(), legacy)
                .expect("pick")
                .0,
            "only-free"
        );
    }
}

/// Not upstream's: a call takes the rule from its metadata, so an image
/// call never reaches a free Codex credential, and a retry decision under
/// the rule doesn't count one.
#[tokio::test(start_paused = true)]
async fn calls_take_the_rule_from_their_metadata() {
    let model = "gpt-image-2";
    let h = Harness::new(Settings {
        request_retry: 2,
        ..Settings::default()
    });
    let executor = FakeExecutor::new("codex");
    h.executor(&executor);
    h.add(cred("a-free", "codex", &[("plan_type", "free")]), &[model]);
    h.add(cred("b-plus", "codex", &[("plan_type", "plus")]), &[model]);
    executor.set_handler(|call| {
        if call.auth.id == "b-plus" {
            Reply::status(429, "slow down")
        } else {
            Reply::ok("ok")
        }
    });

    let mut opts = options();
    opts.metadata.disallow_free_auth = true;
    let err = h
        .manager
        .execute(&providers(&["codex"]), request(model), opts)
        .await
        .expect_err("only the paid credential, which fails");
    assert_eq!(err.status, 429);
    assert_eq!(executor.ids(Kind::Execute), ["b-plus"]);

    // After that round, only the free credential could start another at
    // once; the rule leaves only the paid one, which waits out its 429.
    let codex = providers(&["codex"]);
    let attempted: HashSet<String> = ["b-plus".to_owned()].into_iter().collect();
    let retry = |eligibility| {
        with_selection(&h, |selection, _| {
            let query = RetryQuery {
                providers: &codex,
                model,
                pinned: "",
                attempt: 0,
                default_retry: 2,
                attempted: &attempted,
                eligibility,
            };
            should_retry_after_error(selection, &query, &err, Duration::ZERO)
        })
    };
    assert_eq!(retry(NO_FREE), None);
    assert_eq!(retry(Eligibility::default()), Some(Duration::ZERO));

    let resp = h
        .manager
        .execute(&providers(&["codex"]), request(model), options())
        .await
        .expect("the free credential answers");
    assert_eq!(resp.payload.as_ref(), b"ok");
}

#[tokio::test(start_paused = true)]
async fn manager_initializes_scheduler_for_built_in_selector() {
    let h = harness(RoutingStrategy::RoundRobin, &["gemini"]);
    h.add(auth("auth-a", "gemini"), &[]);
    h.add(auth("auth-b", "gemini"), &[]);
    assert_eq!(
        h.manager.settings().routing_strategy,
        RoutingStrategy::RoundRobin
    );
    assert_eq!(
        [pick(&h, "gemini", ""), pick(&h, "gemini", "")],
        ["auth-a", "auth-b"]
    );

    h.manager.set_settings(Settings {
        routing_strategy: RoutingStrategy::FillFirst,
        ..Settings::default()
    });
    assert_eq!(
        h.manager.settings().routing_strategy,
        RoutingStrategy::FillFirst
    );
    assert_eq!(
        [pick(&h, "gemini", ""), pick(&h, "gemini", "")],
        ["auth-a", "auth-a"]
    );
}

#[tokio::test(start_paused = true)]
async fn manager_scheduler_tracks_register_and_update() {
    let h = harness(RoutingStrategy::RoundRobin, &["gemini"]);
    h.add(auth("auth-b", "gemini"), &[]);
    h.add(auth("auth-a", "gemini"), &[]);

    assert_eq!(pick(&h, "gemini", ""), "auth-a");

    let mut disabled = auth("auth-a", "gemini");
    disabled.disabled = true;
    h.manager
        .update(disabled)
        .expect("update")
        .expect("credential registered");
    assert_eq!(pick(&h, "gemini", ""), "auth-b", "pick after update");
}

#[tokio::test(start_paused = true)]
async fn manager_pick_next_mixed_uses_scheduler_rotation() {
    let h = harness(RoutingStrategy::RoundRobin, &["gemini", "claude"]);
    h.add(auth("gemini-a", "gemini"), &[]);
    h.add(auth("gemini-b", "gemini"), &[]);
    h.add(auth("claude-a", "claude"), &[]);

    assert_provider_rotation(&h);
}

#[tokio::test(start_paused = true)]
async fn manager_scheduler_shares_thinking_suffix_cooldown_and_registry_state() {
    let base_model = "scheduler-thinking-model";
    let clients = ["thinking-auth-a", "thinking-auth-b"];
    let h = harness(RoutingStrategy::RoundRobin, &["gemini"]);
    h.add(auth("thinking-auth-a", "gemini"), &[base_model]);
    h.add(auth("thinking-auth-b", "gemini"), &[base_model]);

    let mut cooled = failure("thinking-auth-a", &format!("{base_model}(high)"), "quota");
    cooled.retry_after = Some(Duration::from_secs(3600));
    h.manager.mark_result(&cooled);

    let auth = h.get("thinking-auth-a");
    assert_eq!(
        auth.model_states.keys().collect::<Vec<_>>(),
        [base_model],
        "want only the canonical key"
    );
    // Only thinking-auth-a is cooling down; thinking-auth-b remains available.
    assert_eq!(
        model_count(&h, &clients, base_model),
        1,
        "registry model count during cooldown"
    );
    for model in [
        base_model.to_owned(),
        format!("{base_model}(medium)"),
        format!("{base_model}(low)"),
    ] {
        assert_eq!(pick(&h, "gemini", &model), "thinking-auth-b", "{model}");
    }

    h.manager
        .mark_result(&success("thinking-auth-a", &format!("{base_model}(low)")));

    let auth = h.get("thinking-auth-a");
    let state = auth
        .model_states
        .get(base_model)
        .expect("canonical model state retained after success");
    assert!(
        !state.unavailable && !state.quota.exceeded && state.next_retry_after.is_none(),
        "canonical model state after success = {state:?}, want cleared"
    );
    assert_eq!(
        model_count(&h, &clients, base_model),
        2,
        "registry model count after recovery"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_pick_next_mixed_skips_providers_without_executors() {
    let h = harness(RoutingStrategy::RoundRobin, &["claude"]);
    h.add(auth("gemini-a", "gemini"), &[]);
    h.add(auth("claude-a", "claude"), &[]);

    let (id, provider) = pick_mixed(&h, &["gemini", "claude"], "", &HashSet::new());
    assert_eq!(provider, "claude");
    assert_eq!(id, "claude-a");
}

#[tokio::test(start_paused = true)]
async fn manager_scheduler_tracks_mark_result_cooldown_and_recovery() {
    let h = harness(RoutingStrategy::RoundRobin, &["gemini"]);
    h.add(auth("auth-a", "gemini"), &["test-model"]);
    h.add(auth("auth-b", "gemini"), &["test-model"]);

    h.manager
        .mark_result(&failure("auth-a", "test-model", "quota"));
    assert_eq!(
        pick(&h, "gemini", "test-model"),
        "auth-b",
        "pick after cooldown"
    );

    h.manager.mark_result(&success("auth-a", "test-model"));
    let seen: HashSet<String> = (0..2).map(|_| pick(&h, "gemini", "test-model")).collect();
    assert_eq!(seen.len(), 2, "picks after recovery: {seen:?}");
}
