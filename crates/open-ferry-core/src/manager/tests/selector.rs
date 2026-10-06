// Ported from CLIProxyAPI sdk/cliproxy/auth/selector_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Picking a credential: fill-first, round-robin and smooth weighted
//! round-robin within the highest ready priority, which credentials are
//! blocked for a model and until when, and the error when every candidate
//! is cooling down.
//!
//! Upstream calls its selector objects on a slice of credentials. Here a
//! [`Pool`] holds the credentials, and each test runs on both paths a pick
//! can take: the scheduler (`pick_next_mixed` with no route-aware
//! credential) and the legacy pick (`pick_next_mixed_legacy`), which is
//! where upstream's selectors run.
//!
//! Deviations from upstream:
//! - Credentials name their provider and serve the test models in the fake
//!   registry, since both paths need an executor and registered models.
//! - A shrinking candidate slice is the pick's `tried` set. Go's zero time
//!   is `None`.
//! - `PickSmoothWeightedAuth_SaturatesCorruptState` runs
//!   `SmoothWeighted::pick`, upstream's `pickSmoothWeightedAuth`.
//! - `RoundRobinSelectorPick_Concurrent` runs 32 Tokio tasks of 100
//!   manager calls each, since the selector state lives in the manager.
//! - `SelectorPick_AllCooldownReturnsModelCooldownError` gets "mixed" from a
//!   legacy pick and from a scheduler pick over two providers, and "gemini"
//!   from a scheduler pick over one.
//! - `RoundRobinSelectorPick_CursorKeyCap` can't set a cap of 2: it fills
//!   the scheduler's cursor map to its fixed cap of 4096 keys, then checks
//!   that one more key starts it over. The legacy round robin keeps one key
//!   ("mixed:"), as upstream's legacy path does.
//! - Dropped: the session-affinity tests (`ExtractSessionID*`,
//!   `ExtractExplicitSessionIDs_*`, `SessionAffinitySelector*`,
//!   `SessionCache*`), by policy; and the `ManagerSetSelector*` tests, since
//!   there are no pluggable selectors (the strategy is a setting).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeDelta, TimeZone, Utc};
use serde_json::{Value, json};

use super::support::*;
use crate::auth::{Auth, ModelState, QuotaState, Status, Timestamp};
use crate::exec::{Dispatcher, ErrorKind, ExecError};
use crate::executor::ProviderExecutor;
use crate::manager::credential::weight;
use crate::manager::models::{OAuthAliasTable, Resolver};
use crate::manager::select::{
    BlockReason, MAX_CURSOR_KEYS, MAX_SMOOTH_WEIGHTED_STATE_ENTRIES, PickArgs, Selection,
    SelectorState, SmoothWeighted, is_auth_blocked_for_model, successor_index,
};
use crate::manager::text::canonical_model_key;
use crate::manager::{Entry, RoutingStrategy, Settings};

/// Which way a pick goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Path {
    /// `pick_next_mixed` with no route-aware credential: the scheduler.
    Scheduler,
    /// `pick_next_mixed_legacy`: upstream's selectors.
    Legacy,
}

const PATHS: [Path; 2] = [Path::Scheduler, Path::Legacy];

/// The models every pool credential serves.
const SERVED: [&str; 2] = ["model", "test-model"];

fn base_now() -> Timestamp {
    Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
        .single()
        .expect("valid time")
}

fn after(now: Timestamp, d: Duration) -> Timestamp {
    now + TimeDelta::from_std(d).expect("duration")
}

fn before(now: Timestamp, d: Duration) -> Timestamp {
    now - TimeDelta::from_std(d).expect("duration")
}

/// A credential with no provider; the pool fills it in.
fn cred(id: &str) -> Auth {
    Auth {
        id: id.to_owned(),
        ..Auth::default()
    }
}

fn with_attr(mut auth: Auth, key: &str, value: &str) -> Auth {
    auth.attributes.insert(key.to_owned(), value.to_owned());
    auth
}

fn weighted(id: &str, weight: &str) -> Auth {
    with_attr(cred(id), "weight", weight)
}

fn with_priority(id: &str, priority: &str) -> Auth {
    with_attr(cred(id), "priority", priority)
}

fn with_state(mut auth: Auth, model: &str, state: ModelState) -> Auth {
    auth.model_states.insert(model.to_owned(), state);
    auth
}

/// Credentials a pick chooses from, and the rotation state picks keep.
struct Pool {
    path: Path,
    provider: String,
    strategy: RoutingStrategy,
    auths: BTreeMap<String, Entry>,
    executors: HashMap<String, Arc<dyn ProviderExecutor>>,
    models: FakeModels,
    settings: Settings,
    oauth: OAuthAliasTable,
    state: SelectorState,
    now: Timestamp,
}

impl Pool {
    fn new(path: Path, strategy: RoutingStrategy, provider: &str) -> Self {
        let mut pool = Self {
            path,
            provider: provider.to_owned(),
            strategy,
            auths: BTreeMap::new(),
            executors: HashMap::new(),
            models: FakeModels::default(),
            settings: Settings::default(),
            oauth: OAuthAliasTable::default(),
            state: SelectorState::default(),
            now: base_now(),
        };
        pool.executor(provider);
        pool
    }

    fn executor(&mut self, provider: &str) {
        let executor: Arc<dyn ProviderExecutor> = FakeExecutor::new(provider);
        self.executors.insert(provider.to_owned(), executor);
    }

    /// Adds `auth`, or replaces the one with its ID. It runs on the pool's
    /// provider unless it names its own, and serves [`SERVED`].
    fn put(&mut self, mut auth: Auth) {
        if auth.provider.is_empty() {
            auth.provider = self.provider.clone();
        }
        self.models.register(&auth.id, &SERVED);
        self.auths.insert(
            auth.id.clone(),
            Entry {
                auth: Arc::new(Auth {
                    registration_epoch: 1,
                    generation: 1,
                    ..auth
                }),
                refresh_failures: 0,
            },
        );
    }

    fn set(&mut self, auths: impl IntoIterator<Item = Auth>) {
        self.auths.clear();
        for auth in auths {
            self.put(auth);
        }
    }

    fn pick(&mut self, model: &str) -> Result<String, ExecError> {
        let provider = self.provider.clone();
        self.pick_from(&[&provider], model, &[])
    }

    fn pick_excluding(&mut self, model: &str, tried: &[&str]) -> Result<String, ExecError> {
        let provider = self.provider.clone();
        self.pick_from(&[&provider], model, tried)
    }

    fn pick_from(
        &mut self,
        providers: &[&str],
        model: &str,
        tried: &[&str],
    ) -> Result<String, ExecError> {
        let providers: Vec<String> = providers.iter().map(|p| (*p).to_owned()).collect();
        let tried: HashSet<String> = tried.iter().map(|id| (*id).to_owned()).collect();
        let resolver = Resolver {
            settings: &self.settings,
            oauth: &self.oauth,
        };
        if self.path == Path::Scheduler && !model.trim().is_empty() {
            // pick_next_mixed only stays on the scheduler while no credential
            // routes the model under another name.
            let route_key = canonical_model_key(model);
            assert!(
                self.auths
                    .values()
                    .all(|e| resolver.selection_model_key_for_auth(&e.auth, model) == route_key),
                "a route-aware credential would take the legacy path"
            );
        }
        let selection = Selection {
            auths: &self.auths,
            executors: &self.executors,
            models: &self.models,
            resolver,
            strategy: self.strategy,
            now: self.now,
        };
        let args = PickArgs {
            model,
            pinned: "",
            downstream_websocket: false,
            eligibility: Default::default(),
            tried: &tried,
        };
        let picked = match self.path {
            Path::Scheduler => selection.pick_next_mixed(&mut self.state, &providers, &args),
            Path::Legacy => selection.pick_next_mixed_legacy(&mut self.state, &providers, &args),
        }?;
        Ok(picked.auth.id.clone())
    }
}

fn counts(ids: impl IntoIterator<Item = String>) -> HashMap<String, usize> {
    let mut out = HashMap::new();
    for id in ids {
        *out.entry(id).or_insert(0) += 1;
    }
    out
}

fn count(counts: &HashMap<String, usize>, id: &str) -> usize {
    counts.get(id).copied().unwrap_or(0)
}

fn pick_n(pool: &mut Pool, model: &str, n: usize) -> HashMap<String, usize> {
    counts((0..n).map(|i| {
        pool.pick(model)
            .unwrap_or_else(|err| panic!("{:?} pick #{i}: {err}", pool.path))
    }))
}

#[test]
fn fill_first_selector_pick_deterministic() {
    for path in PATHS {
        let mut pool = Pool::new(path, RoutingStrategy::FillFirst, "gemini");
        pool.set([cred("b"), cred("a"), cred("c")]);
        assert_eq!(pool.pick("").expect("pick"), "a", "{path:?}");
    }
}

#[test]
fn round_robin_selector_pick_cycles_deterministic() {
    for path in PATHS {
        let mut pool = Pool::new(path, RoutingStrategy::RoundRobin, "gemini");
        pool.set([cred("b"), cred("a"), cred("c")]);
        for (i, want) in ["a", "b", "c", "a", "b"].iter().enumerate() {
            assert_eq!(pool.pick("").expect("pick"), *want, "{path:?} pick #{i}");
        }
    }
}

#[test]
fn weighted_round_robin_selector_pick_distributes_and_skips_non_positive_weights() {
    for path in PATHS {
        let mut pool = Pool::new(path, RoutingStrategy::Weighted, "gemini");
        pool.set([
            weighted("a", "5"),
            weighted("b", "3"),
            weighted("c", "2"),
            weighted("disabled-by-weight", "0"),
        ]);
        let got = pick_n(&mut pool, "model", 100);
        for (id, want) in [("a", 50), ("b", 30), ("c", 20)] {
            assert_eq!(count(&got, id), want, "{path:?} auth {id} picks");
        }
        assert_eq!(count(&got, "disabled-by-weight"), 0, "{path:?}");
    }
}

#[test]
fn weighted_round_robin_selector_pick_resets_credits_when_weights_change() {
    for path in PATHS {
        let mut pool = Pool::new(path, RoutingStrategy::Weighted, "gemini");
        pool.set([weighted("a", "1000000"), weighted("b", "1")]);
        pick_n(&mut pool, "model", 1000);

        pool.put(weighted("a", "1"));
        let got = pick_n(&mut pool, "model", 20);
        assert_eq!(
            (count(&got, "a"), count(&got, "b")),
            (10, 10),
            "{path:?} picks after weight change"
        );
    }
}

#[test]
fn weighted_round_robin_selector_pick_rebalances_when_highest_weight_unavailable() {
    for path in PATHS {
        let mut pool = Pool::new(path, RoutingStrategy::Weighted, "gemini");
        let mut a = weighted("a", "5");
        a.disabled = true;
        pool.set([a, weighted("b", "3"), weighted("c", "2")]);
        let got = pick_n(&mut pool, "model", 100);
        assert_eq!(
            (count(&got, "a"), count(&got, "b"), count(&got, "c")),
            (0, 60, 40),
            "{path:?} weighted failover counts"
        );
    }
}

#[test]
fn weighted_round_robin_selector_pick_skips_unavailable_and_quota_exceeded_without_recovery() {
    let model = "test-model";
    for path in PATHS {
        let mut pool = Pool::new(path, RoutingStrategy::Weighted, "gemini");
        let model_unavailable = with_state(
            cred("model-unavailable"),
            model,
            ModelState {
                unavailable: true,
                ..ModelState::default()
            },
        );
        let mut quota_exceeded = cred("quota-exceeded");
        quota_exceeded.quota.exceeded = true;
        pool.set([model_unavailable, quota_exceeded, cred("available")]);

        assert_eq!(
            pool.pick(model).expect("model pick"),
            "available",
            "{path:?}"
        );
        for i in 0..4 {
            let got = pool
                .pick("")
                .unwrap_or_else(|err| panic!("{path:?} auth pick #{i}: {err}"));
            assert_ne!(got, "quota-exceeded", "{path:?} auth pick #{i}");
        }
    }
}

#[test]
fn auth_weight_metadata_fallback_and_attribute_precedence() {
    let mut auth = cred("a");
    auth.metadata.insert("weight".into(), json!(7.0));
    assert_eq!(weight(&auth), 7, "metadata weight");
    let auth = with_attr(auth, "weight", "3");
    assert_eq!(weight(&auth), 3, "attribute wins over metadata");
}

#[test]
fn auth_weight_invalid_and_overflow_values_are_excluded() {
    for raw in [
        "1.5",
        "1000001",
        "9223372036854775807",
        "9223372036854775808",
    ] {
        assert_eq!(weight(&weighted("a", raw)), 0, "weight({raw:?})");
    }
    let mut auth = cred("a");
    auth.metadata.insert("weight".into(), json!(1.5));
    assert_eq!(weight(&auth), 0, "invalid metadata");
    assert_eq!(weight(&weighted("a", "-1")), 0, "weight(-1)");
}

#[test]
fn pick_smooth_weighted_auth_saturates_corrupt_state() {
    let mut state = SmoothWeighted {
        current: Some(HashMap::from([
            ("a".to_owned(), i64::MAX),
            ("b".to_owned(), i64::MIN),
        ])),
        weights: HashMap::new(),
    };
    let auths = [cred("a"), cred("b")];
    let picked = state.pick(auths.iter().map(|a| (a.id.as_str(), weight(a))));
    assert!(picked.is_some(), "pick returned none");
    let current = state.current.expect("credits");
    assert_eq!(current["a"], i64::MAX - 2);
    assert_eq!(current["b"], i64::MIN + 1);
}

#[test]
fn weighted_round_robin_selector_pick_recovered_auth_returns_without_accumulated_credit() {
    for path in PATHS {
        let mut pool = Pool::new(path, RoutingStrategy::Weighted, "gemini");
        pool.set([weighted("a", "5"), weighted("b", "1")]);
        pick_n(&mut pool, "model", 6);

        let mut a = weighted("a", "5");
        a.unavailable = true;
        a.next_retry_after = Some(after(pool.now, Duration::from_secs(3600)));
        pool.put(a);
        for i in 0..6 {
            assert_eq!(
                pool.pick("model").expect("pick"),
                "b",
                "{path:?} unavailable pick #{i}"
            );
        }

        pool.put(weighted("a", "5"));
        let got = pick_n(&mut pool, "model", 6);
        assert_eq!(
            (count(&got, "a"), count(&got, "b")),
            (5, 1),
            "{path:?} recovered picks"
        );
    }
}

#[test]
fn weighted_round_robin_selector_pick_default_weight_is_one() {
    for path in PATHS {
        let mut pool = Pool::new(path, RoutingStrategy::Weighted, "gemini");
        pool.set([cred("a"), cred("b"), cred("c")]);
        let got = pick_n(&mut pool, "model", 30);
        for id in ["a", "b", "c"] {
            assert_eq!(count(&got, id), 10, "{path:?} auth {id} picks");
        }
    }
}

#[test]
fn weighted_round_robin_selector_pick_subset_filtering_does_not_reset_accumulator_or_favor_first_alphabetical()
 {
    for path in PATHS {
        let mut pool = Pool::new(path, RoutingStrategy::Weighted, "provider");
        pool.set([
            cred("auth-a"),
            cred("auth-b"),
            cred("auth-c"),
            cred("auth-d"),
        ]);
        // auth-a is left out of every pick, as a tried or cooling credential.
        let got = counts((0..30).map(|i| {
            pool.pick_excluding("model", &["auth-a"])
                .unwrap_or_else(|err| panic!("{path:?} pick #{i}: {err}"))
        }));
        for id in ["auth-b", "auth-c", "auth-d"] {
            assert_eq!(count(&got, id), 10, "{path:?} auth {id} picks: {got:?}");
        }
    }
}

#[test]
fn smooth_weighted_state_prepare_keeps_credits_for_transient_subsets_and_bounds_growth() {
    let weights = |pairs: &[(&str, i64)]| -> HashMap<String, i64> {
        pairs.iter().map(|(id, w)| ((*id).to_owned(), *w)).collect()
    };
    let mut state = SmoothWeighted::default();
    state.prepare(&weights(&[("a", 1), ("b", 1)]));
    let current = state.current.as_mut().expect("credits");
    current.insert("a".into(), -2);
    current.insert("b".into(), 1);

    // A shrinking candidate set keeps the credits.
    state.prepare(&weights(&[("b", 1)]));
    let current = state.current.as_ref().expect("credits");
    assert_eq!((current.get("a"), current.get("b")), (Some(&-2), Some(&1)));

    // A real weight change resets them.
    state.prepare(&weights(&[("b", 5)]));
    assert_eq!(state.current.as_ref().map_or(0, HashMap::len), 0);

    // Churn stays bounded.
    for i in 0..MAX_SMOOTH_WEIGHTED_STATE_ENTRIES * 3 {
        state.prepare(&weights(&[(&format!("churn-{i}"), 5)]));
    }
    let current_len = state.current.as_ref().map_or(0, HashMap::len);
    assert!(
        current_len <= MAX_SMOOTH_WEIGHTED_STATE_ENTRIES
            && state.weights.len() <= MAX_SMOOTH_WEIGHTED_STATE_ENTRIES,
        "state grew unbounded: current={current_len} weights={}",
        state.weights.len()
    );
}

#[test]
fn round_robin_selector_pick_priority_buckets() {
    for path in PATHS {
        let mut pool = Pool::new(path, RoutingStrategy::RoundRobin, "gemini");
        pool.set([
            with_priority("c", "0"),
            with_priority("a", "10"),
            with_priority("b", "10"),
        ]);
        for (i, want) in ["a", "b", "a", "b"].iter().enumerate() {
            assert_eq!(pool.pick("").expect("pick"), *want, "{path:?} pick #{i}");
        }
    }
}

#[test]
fn fill_first_selector_pick_priority_fallback_cooldown() {
    let model = "test-model";
    for path in PATHS {
        let mut pool = Pool::new(path, RoutingStrategy::FillFirst, "gemini");
        let high = with_state(
            with_priority("high", "10"),
            model,
            ModelState {
                status: Status::Active,
                unavailable: true,
                next_retry_after: Some(after(pool.now, Duration::from_secs(30 * 60))),
                quota: QuotaState {
                    exceeded: true,
                    ..QuotaState::default()
                },
                ..ModelState::default()
            },
        );
        pool.set([high, with_priority("low", "0")]);
        assert_eq!(pool.pick(model).expect("pick"), "low", "{path:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn round_robin_selector_pick_concurrent() {
    const TASKS: usize = 32;
    const ITERATIONS: usize = 100;
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::new("gemini");
    h.executor(&executor);
    for id in ["b", "a", "c"] {
        h.add(auth(id, "gemini"), &[]);
    }

    let mut tasks = Vec::with_capacity(TASKS);
    for _ in 0..TASKS {
        let manager = h.manager.clone();
        tasks.push(tokio::spawn(async move {
            let providers = providers(&["gemini"]);
            for j in 0..ITERATIONS {
                manager
                    .execute(&providers, request(""), options())
                    .await
                    .unwrap_or_else(|err| panic!("concurrent pick #{j}: {err}"));
            }
        }));
    }
    for task in tasks {
        task.await.expect("task");
    }
    let ids = executor.ids(Kind::Execute);
    assert_eq!(ids.len(), TASKS * ITERATIONS);
    assert!(ids.iter().all(|id| !id.is_empty()), "pick with empty ID");
}

/// The error body of a model cooldown error.
fn cooldown_body(err: &ExecError) -> serde_json::Map<String, Value> {
    let payload: Value = serde_json::from_str(&err.to_string()).expect("JSON error text");
    match payload.get("error") {
        Some(Value::Object(body)) => body.clone(),
        _ => panic!("error text missing error object: {payload}"),
    }
}

#[test]
fn selector_pick_all_cooldown_returns_model_cooldown_error() {
    let model = "test-model";
    let cooling = |pool: &Pool, id: &str| {
        let next = after(pool.now, Duration::from_secs(60));
        with_state(
            cred(id),
            model,
            ModelState {
                status: Status::Active,
                unavailable: true,
                next_retry_after: Some(next),
                quota: QuotaState {
                    exceeded: true,
                    next_recover_at: Some(next),
                    ..QuotaState::default()
                },
                ..ModelState::default()
            },
        )
    };

    // Mixed provider redacts the provider field: a legacy pick, and a
    // scheduler pick over two providers.
    for (path, providers) in [
        (Path::Legacy, &["gemini"][..]),
        (Path::Scheduler, &["gemini", "claude"][..]),
    ] {
        let mut pool = Pool::new(path, RoutingStrategy::FillFirst, "gemini");
        pool.executor("claude");
        let auths = [cooling(&pool, "a"), cooling(&pool, "b")];
        pool.set(auths);
        let err = pool
            .pick_from(providers, model, &[])
            .expect_err("all cooling down");
        assert_eq!(err.kind, ErrorKind::ModelCooldown, "{path:?}");
        assert_eq!(err.http_status(), 429, "{path:?}");
        let retry_after = err
            .headers
            .get(http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert_eq!(retry_after, "60", "{path:?} Retry-After");
        let body = cooldown_body(&err);
        assert_eq!(body.get("code"), Some(&json!("model_cooldown")), "{path:?}");
        assert!(
            !body.contains_key("provider"),
            "{path:?} provider field for mixed provider: {body:?}"
        );
    }

    // A single provider includes it.
    let mut pool = Pool::new(Path::Scheduler, RoutingStrategy::FillFirst, "gemini");
    let auths = [cooling(&pool, "a"), cooling(&pool, "b")];
    pool.set(auths);
    let err = pool.pick(model).expect_err("all cooling down");
    assert_eq!(err.kind, ErrorKind::ModelCooldown);
    let body = cooldown_body(&err);
    assert_eq!(body.get("provider"), Some(&json!("gemini")));
}

#[test]
fn is_auth_blocked_for_model_unavailable_without_next_retry_is_blocked() {
    let model = "test-model";
    let auth = with_state(
        cred("a"),
        model,
        ModelState {
            status: Status::Active,
            unavailable: true,
            quota: QuotaState {
                exceeded: true,
                ..QuotaState::default()
            },
            ..ModelState::default()
        },
    );
    assert_eq!(
        is_auth_blocked_for_model(&auth, model, base_now()),
        (true, BlockReason::Other, None)
    );
}

#[test]
fn is_auth_blocked_for_model_auth_quota_exceeded_without_recovery_is_blocked() {
    let mut auth = cred("a");
    auth.quota.exceeded = true;
    for model in ["", "test-model"] {
        assert_eq!(
            is_auth_blocked_for_model(&auth, model, base_now()),
            (true, BlockReason::Other, None),
            "model {model:?}"
        );
    }
}

#[test]
fn is_auth_blocked_for_model_expired_recovery_is_available() {
    let now = base_now();
    let mut auth = cred("a");
    auth.unavailable = true;
    auth.next_retry_after = Some(before(now, Duration::from_secs(60)));
    auth.quota = QuotaState {
        exceeded: true,
        next_recover_at: Some(before(now, Duration::from_secs(1))),
        ..QuotaState::default()
    };
    assert_eq!(
        is_auth_blocked_for_model(&auth, "", now),
        (false, BlockReason::None, None)
    );
}

#[test]
fn fill_first_selector_pick_thinking_suffix_falls_back_to_base_model_state() {
    for path in PATHS {
        let mut pool = Pool::new(path, RoutingStrategy::FillFirst, "gemini");
        let high = with_state(
            with_priority("high", "10"),
            "test-model",
            ModelState {
                status: Status::Active,
                unavailable: true,
                next_retry_after: Some(after(pool.now, Duration::from_secs(30 * 60))),
                quota: QuotaState {
                    exceeded: true,
                    ..QuotaState::default()
                },
                ..ModelState::default()
            },
        );
        pool.set([high, with_priority("low", "0")]);
        assert_eq!(
            pool.pick("test-model(high)").expect("pick"),
            "low",
            "{path:?}"
        );
    }
}

#[test]
fn is_auth_blocked_for_model_thinking_suffix_states_block_canonical_model() {
    let now = base_now();
    let hour = after(now, Duration::from_secs(3600));
    let later_retry = after(now, Duration::from_secs(2 * 3600));
    let state = |next: Timestamp| ModelState {
        status: Status::Error,
        unavailable: true,
        next_retry_after: Some(next),
        quota: QuotaState {
            exceeded: true,
            next_recover_at: Some(next),
            ..QuotaState::default()
        },
        ..ModelState::default()
    };
    let auth = with_state(
        with_state(cred("a"), "test-model(high)", state(hour)),
        "test-model(low)",
        state(later_retry),
    );
    for model in ["test-model", "test-model(medium)", "test-model(low)"] {
        assert_eq!(
            is_auth_blocked_for_model(&auth, model, now),
            (true, BlockReason::Cooldown, Some(later_retry)),
            "model {model:?}"
        );
    }
}

#[test]
fn round_robin_selector_pick_thinking_suffix_shares_cursor() {
    for path in PATHS {
        let mut pool = Pool::new(path, RoutingStrategy::RoundRobin, "gemini");
        pool.set([cred("b"), cred("a")]);
        let first = pool.pick("test-model(high)").expect("first pick");
        let second = pool.pick("test-model(low)").expect("second pick");
        assert_eq!((first.as_str(), second.as_str()), ("a", "b"), "{path:?}");
    }
}

#[test]
fn round_robin_selector_pick_resumes_rotation_across_retry_exclusions() {
    const REQUESTS: usize = 50;
    let ids = ["aaa", "bbb", "ccc", "ddd", "eee"];
    for path in PATHS {
        let mut pool = Pool::new(path, RoutingStrategy::RoundRobin, "gemini");
        pool.set(ids.map(cred));
        // Every request burns three attempts, so each attempt takes the next
        // slot of one shared rotation.
        let mut first_attempt = HashMap::new();
        let mut all_attempts = HashMap::new();
        for request in 0..REQUESTS {
            let mut tried: Vec<String> = Vec::new();
            for attempt in 0..3 {
                let excluded: Vec<&str> = tried.iter().map(String::as_str).collect();
                let got = pool
                    .pick_excluding("model", &excluded)
                    .unwrap_or_else(|err| {
                        panic!("{path:?} request {request} attempt {attempt}: {err}")
                    });
                if attempt == 0 {
                    *first_attempt.entry(got.clone()).or_insert(0) += 1;
                }
                *all_attempts.entry(got.clone()).or_insert(0) += 1;
                tried.push(got);
            }
        }
        for id in ids {
            assert_eq!(
                count(&first_attempt, id),
                REQUESTS / ids.len(),
                "{path:?} auth {id} first attempts: {first_attempt:?}"
            );
            assert_eq!(
                count(&all_attempts, id),
                REQUESTS * 3 / ids.len(),
                "{path:?} auth {id} total attempts: {all_attempts:?}"
            );
        }
    }
}

#[test]
fn weighted_round_robin_selector_pick_keeps_weight_ratios_when_candidates_are_excluded() {
    for path in PATHS {
        let mut pool = Pool::new(path, RoutingStrategy::Weighted, "codex");
        pool.set([
            weighted("auth-a", "5"),
            weighted("auth-b", "3"),
            weighted("auth-c", "1"),
        ]);
        pick_n(&mut pool, "model", 9);

        // auth-a is left out as a retry or cooldown would leave it out; the
        // others keep their 3:1 ratio.
        let got = counts((0..400).map(|i| {
            pool.pick_excluding("model", &["auth-a"])
                .unwrap_or_else(|err| panic!("{path:?} survivor pick #{i}: {err}"))
        }));
        assert_eq!(
            (count(&got, "auth-b"), count(&got, "auth-c")),
            (300, 100),
            "{path:?} survivor picks: {got:?}"
        );
    }
}

#[test]
fn successor_index_wraps_and_skips_filtered_candidates() {
    let available = ["aaa", "ccc", "eee"];
    for (name, last, want) in [
        ("no previous pick starts at head", "", 0),
        ("resumes after previous pick", "aaa", 1),
        ("resumes after filtered-out pick", "bbb", 1),
        ("wraps at the end of the ring", "eee", 0),
        ("wraps for removed trailing pick", "zzz", 0),
    ] {
        assert_eq!(successor_index(&available, last), want, "{name}");
    }
}

#[test]
fn round_robin_selector_pick_cursor_key_cap() {
    let mut pool = Pool::new(Path::Scheduler, RoutingStrategy::RoundRobin, "gemini");
    pool.set([cred("a")]);
    let pick = |pool: &mut Pool, i: usize| {
        let model = format!("m{i}");
        pool.models.register("a", &[&model]);
        pool.pick(&model).expect("pick");
    };
    for i in 1..=MAX_CURSOR_KEYS {
        pick(&mut pool, i);
    }
    assert_eq!(pool.state.shards.len(), MAX_CURSOR_KEYS, "map at the cap");

    // One key past the cap starts the map over.
    let last = format!("m{}", MAX_CURSOR_KEYS + 1);
    pick(&mut pool, MAX_CURSOR_KEYS + 1);
    assert_eq!(pool.state.shards.len(), 1);
    assert!(
        pool.state
            .shards
            .contains_key(&("gemini".to_owned(), last.clone())),
        "cursor map missing key gemini:{last}"
    );
}
