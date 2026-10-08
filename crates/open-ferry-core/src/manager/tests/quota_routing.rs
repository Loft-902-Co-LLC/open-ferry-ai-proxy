//! Not upstream's: open-ferry's `quota` strategy (see `quota_rank`), which
//! CLIProxyAPI doesn't have, so nothing here is ported and there is no
//! parity suite for it.
//!
//! Each order is checked on both paths a pick can take, the scheduler and
//! the legacy pick, by picking again with the credentials picked so far
//! tried; and on the session affinity path for its bindings. Readings are
//! set on the credentials as the manager records them from responses.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeDelta, TimeZone, Utc};

use super::support::*;
use crate::auth::{Auth, QuotaState, Timestamp};
use crate::exec::{Dispatcher, ExecError};
use crate::executor::ProviderExecutor;
use crate::manager::affinity::{Affinity, Session};
use crate::manager::models::{OAuthAliasTable, Resolver};
use crate::manager::select::{PickArgs, Selection, SelectorState};
use crate::manager::{Entry, QuotaPreference, QuotaPrefs, RoutingStrategy, Settings};
use crate::session::Payload;

/// Which way a pick goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Path {
    Scheduler,
    Legacy,
}

const PATHS: [Path; 2] = [Path::Scheduler, Path::Legacy];

const MODEL: &str = "model";

fn now() -> Timestamp {
    Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
        .single()
        .expect("valid time")
}

fn quota(prefer: QuotaPreference, reserve_percent: u8) -> RoutingStrategy {
    RoutingStrategy::Quota(QuotaPrefs {
        prefer,
        reserve_percent,
    })
}

const SOONEST: QuotaPreference = QuotaPreference::SoonestReset;
const MOST_LEFT: QuotaPreference = QuotaPreference::MostLeft;

/// A credential with no reading.
fn cred(id: &str) -> Auth {
    Auth {
        id: id.to_owned(),
        ..Auth::default()
    }
}

/// A credential whose last reading came a minute ago with `signals`.
fn read(id: &str, signals: &[(&str, String)]) -> Auth {
    Auth {
        quota: QuotaState {
            observed_at: Some(now() - TimeDelta::minutes(1)),
            signals: signals
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.clone()))
                .collect(),
            ..QuotaState::default()
        },
        ..cred(id)
    }
}

/// Unix seconds `minutes` from now.
fn at(minutes: i64) -> String {
    (now() + TimeDelta::minutes(minutes))
        .timestamp()
        .to_string()
}

/// A Claude credential whose 5-hour window is `used` percent used and
/// resets in `resets_in` minutes.
fn claude(id: &str, used: u32, resets_in: i64) -> Auth {
    read(
        id,
        &[
            (
                "Anthropic-Ratelimit-Unified-5h-Utilization",
                format!("{}", f64::from(used) / 100.0),
            ),
            ("Anthropic-Ratelimit-Unified-5h-Reset", at(resets_in)),
        ],
    )
}

/// A Claude credential with both windows: `(used, resets_in)` each.
fn claude_both(id: &str, five: (u32, i64), seven: (u32, i64)) -> Auth {
    read(
        id,
        &[
            (
                "Anthropic-Ratelimit-Unified-5h-Utilization",
                format!("{}", f64::from(five.0) / 100.0),
            ),
            ("Anthropic-Ratelimit-Unified-5h-Reset", at(five.1)),
            (
                "Anthropic-Ratelimit-Unified-7d-Utilization",
                format!("{}", f64::from(seven.0) / 100.0),
            ),
            ("Anthropic-Ratelimit-Unified-7d-Reset", at(seven.1)),
        ],
    )
}

/// A Codex credential whose primary window is `used` percent used and
/// resets in `resets_in` minutes.
fn codex(id: &str, used: u32, resets_in: i64) -> Auth {
    let mut auth = read(
        id,
        &[
            ("X-Codex-Primary-Used-Percent", used.to_string()),
            ("X-Codex-Primary-Reset-At", at(resets_in)),
        ],
    );
    auth.provider = "codex".into();
    auth
}

fn with_attr(mut auth: Auth, key: &str, value: &str) -> Auth {
    auth.attributes.insert(key.to_owned(), value.to_owned());
    auth
}

/// Credentials a pick chooses from, and the state picks keep.
struct Pool {
    strategy: RoutingStrategy,
    auths: BTreeMap<String, Entry>,
    executors: HashMap<String, Arc<dyn ProviderExecutor>>,
    models: FakeModels,
    settings: Settings,
    oauth: OAuthAliasTable,
    state: SelectorState,
    affinity: Affinity,
    now: Timestamp,
}

impl Pool {
    fn new(strategy: RoutingStrategy) -> Self {
        let mut executors: HashMap<String, Arc<dyn ProviderExecutor>> = HashMap::new();
        for provider in ["claude", "codex"] {
            let executor: Arc<dyn ProviderExecutor> = FakeExecutor::new(provider);
            executors.insert(provider.to_owned(), executor);
        }
        Self {
            strategy,
            auths: BTreeMap::new(),
            executors,
            models: FakeModels::default(),
            settings: Settings::default(),
            oauth: OAuthAliasTable::default(),
            state: SelectorState::default(),
            affinity: Affinity::new(Duration::from_secs(60 * 60), true),
            now: now(),
        }
    }

    fn of(strategy: RoutingStrategy, auths: impl IntoIterator<Item = Auth>) -> Self {
        let mut pool = Self::new(strategy);
        for auth in auths {
            pool.put(auth);
        }
        pool
    }

    /// Adds `auth`, or replaces the one with its ID; it runs on Claude
    /// unless it names its provider.
    fn put(&mut self, mut auth: Auth) {
        if auth.provider.is_empty() {
            auth.provider = "claude".into();
        }
        self.models.register(&auth.id, &[MODEL]);
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

    fn selection(&self) -> Selection<'_> {
        Selection {
            auths: &self.auths,
            executors: &self.executors,
            models: &self.models,
            resolver: Resolver {
                settings: &self.settings,
                oauth: &self.oauth,
            },
            strategy: self.strategy,
            now: self.now,
        }
    }

    fn pick_from(
        &mut self,
        path: Path,
        providers: &[&str],
        tried: &[String],
    ) -> Result<String, ExecError> {
        let providers: Vec<String> = providers.iter().map(|p| (*p).to_owned()).collect();
        let tried: HashSet<String> = tried.iter().cloned().collect();
        let args = PickArgs {
            model: MODEL,
            pinned: "",
            downstream_websocket: false,
            eligibility: Default::default(),
            tried: &tried,
        };
        let mut state = std::mem::take(&mut self.state);
        let selection = self.selection();
        let picked = match path {
            Path::Scheduler => selection.pick_next_mixed(&mut state, &providers, &args),
            Path::Legacy => selection.pick_next_mixed_legacy(&mut state, &providers, &args),
        };
        self.state = state;
        Ok(picked?.auth.id.clone())
    }

    fn pick(&mut self, path: Path) -> String {
        self.pick_from(path, &["claude"], &[])
            .unwrap_or_else(|err| panic!("{path:?}: {err}"))
    }

    /// The order picks go in on `path` across `providers`: each pick with
    /// those before it tried.
    fn order_across(&mut self, path: Path, providers: &[&str]) -> Vec<String> {
        let mut tried = Vec::new();
        while let Ok(id) = self.pick_from(path, providers, &tried) {
            tried.push(id);
        }
        tried
    }

    fn order(&mut self, path: Path) -> Vec<String> {
        self.order_across(path, &["claude"])
    }

    /// The session affinity pick for `session` among every credential.
    fn sticky(&mut self, session: &Session) -> String {
        let candidates: Vec<&Arc<Auth>> = self.auths.values().map(|e| &e.auth).collect();
        let mut state = std::mem::take(&mut self.state);
        let mut affinity = std::mem::replace(
            &mut self.affinity,
            Affinity::new(Duration::from_secs(1), true),
        );
        let picked = self
            .selection()
            .pick_sticky(
                &mut state,
                &mut affinity,
                &candidates,
                "mixed",
                MODEL,
                Some(session),
            )
            .map(|auth| auth.id.clone());
        self.state = state;
        self.affinity = affinity;
        picked.unwrap_or_else(|err| panic!("sticky pick: {err}"))
    }
}

fn session(id: &str) -> Session {
    let body = format!(r#"{{"metadata":{{"user_id":"user_x_account__session_{id}"}}}}"#);
    Session::with_derived(
        &http::HeaderMap::new(),
        &Payload::parse(body.as_bytes()),
        "",
        "",
    )
    .expect("a session")
}

/// Four credentials: `a` 30% used, resetting in 4 hours; `b` 50%, in an
/// hour; `c` with no reading; `d` 10%, in 2 hours.
fn four() -> Vec<Auth> {
    vec![
        claude("a", 30, 240),
        claude("b", 50, 60),
        cred("c"),
        claude("d", 10, 120),
    ]
}

#[test]
fn soonest_reset_uses_the_allowance_about_to_go_first() {
    for path in PATHS {
        let mut pool = Pool::of(quota(SOONEST, 0), four());
        // Known resets in order, then the credential with no reading.
        assert_eq!(pool.order(path), ["b", "d", "a", "c"], "{path:?}");
    }
}

#[test]
fn most_left_takes_the_credential_with_most_left() {
    for path in PATHS {
        let mut pool = Pool::of(quota(MOST_LEFT, 0), four());
        // No reading counts as all of it left.
        assert_eq!(pool.order(path), ["c", "d", "a", "b"], "{path:?}");
    }
}

#[test]
fn the_tightest_window_binds() {
    for path in PATHS {
        // `a`'s 7-day window is fuller than its 5-hour one, so it binds:
        // 40% left, resetting in two days.
        let mut pool = Pool::of(
            quota(MOST_LEFT, 0),
            [
                claude_both("a", (5, 30), (60, 2 * 24 * 60)),
                claude("b", 50, 300),
            ],
        );
        assert_eq!(pool.order(path), ["b", "a"], "{path:?}");
        pool.strategy = quota(SOONEST, 0);
        assert_eq!(pool.order(path), ["b", "a"], "{path:?}");
    }
}

#[test]
fn the_reserve_is_kept_while_another_has_room() {
    for path in PATHS {
        // With 20% kept back, `a` (85% used) and `e` (its 7-day window 80%
        // used) have no room, though they reset first.
        let mut pool = Pool::of(
            quota(SOONEST, 20),
            [
                claude("a", 85, 10),
                claude("b", 50, 180),
                claude_both("e", (0, 5), (80, 20)),
                cred("c"),
            ],
        );
        // Without room, the most left first: `e`'s binding 7-day window has
        // 20% left.
        assert_eq!(pool.order(path), ["b", "c", "e", "a"], "{path:?}");

        // At exactly 100 - reserve there is no room either.
        pool.put(claude("b", 80, 180));
        pool.put(claude("c", 79, 200));
        assert_eq!(pool.order(path)[0], "c", "{path:?}");

        // With no reserve, a full window still has no room.
        pool.strategy = quota(SOONEST, 0);
        pool.put(claude("a", 100, 10));
        pool.put(claude("b", 99, 180));
        assert_eq!(pool.order(path)[..2], ["e", "b"], "{path:?}");
    }
}

#[test]
fn when_none_has_room_the_most_left_serves() {
    for prefer in [SOONEST, MOST_LEFT] {
        for path in PATHS {
            let mut pool = Pool::of(
                quota(prefer, 10),
                [
                    claude("a", 95, 10),
                    claude("b", 91, 300),
                    claude("c", 95, 5),
                    codex("x", 100, 60),
                ],
            );
            // The most left first, then the soonest reset; a full window
            // still serves last.
            assert_eq!(
                pool.order_across(path, &["claude", "codex"]),
                ["b", "c", "a", "x"],
                "{prefer:?} {path:?}"
            );
        }
    }
}

#[test]
fn stale_readings_count_as_none() {
    // `a`'s window reset ten minutes ago; `z`'s reading has no reset.
    let auths = || {
        [
            claude("a", 99, -10),
            claude("b", 40, 60),
            read(
                "z",
                &[("Anthropic-Ratelimit-Unified-5h-Utilization", "0.99".into())],
            ),
        ]
    };
    for path in PATHS {
        let mut pool = Pool::of(quota(SOONEST, 10), auths());
        let order = pool.order(path);
        assert_eq!(order[0], "b", "{path:?}");
        let mut rest = order[1..].to_vec();
        rest.sort();
        assert_eq!(rest, ["a", "z"], "{path:?}");

        let mut pool = Pool::of(quota(MOST_LEFT, 10), auths());
        assert_eq!(pool.order(path), ["a", "z", "b"], "{path:?}");
    }
}

#[test]
fn credentials_that_rank_the_same_take_turns() {
    for path in PATHS {
        let mut pool = Pool::of(quota(SOONEST, 0), [cred("b"), cred("a"), cred("c")]);
        let picks: Vec<String> = (0..4).map(|_| pool.pick(path)).collect();
        assert_eq!(picks, ["a", "b", "c", "a"], "{path:?}");

        // Two with the same reading take turns, ahead of the rest.
        let mut pool = Pool::of(
            quota(MOST_LEFT, 0),
            [
                claude("a", 20, 60),
                claude("b", 20, 60),
                claude("c", 50, 60),
            ],
        );
        let picks: Vec<String> = (0..4).map(|_| pool.pick(path)).collect();
        assert_eq!(picks, ["a", "b", "a", "b"], "{path:?}");
    }
}

#[test]
fn credential_priority_comes_first() {
    for path in PATHS {
        // `a` and `b` have priority 10: even out of room, they come before
        // `c`, which has all of its allowance at priority 0.
        let mut pool = Pool::of(
            quota(MOST_LEFT, 10),
            [
                with_attr(claude("a", 95, 60), "priority", "10"),
                with_attr(claude("b", 97, 60), "priority", "10"),
                claude("c", 0, 60),
            ],
        );
        assert_eq!(pool.pick(path), "a", "{path:?}");
        assert_eq!(
            pool.pick_from(path, &["claude"], &["a".into()])
                .expect("pick"),
            "b",
            "{path:?}"
        );
        assert_eq!(pool.order(path), ["a", "b", "c"], "{path:?}");
    }
}

#[test]
fn weights_are_ignored() {
    for path in PATHS {
        let mut pool = Pool::of(
            quota(MOST_LEFT, 0),
            [
                with_attr(claude("a", 10, 60), "weight", "0"),
                with_attr(claude("b", 50, 60), "weight", "100"),
            ],
        );
        assert_eq!(pool.order(path), ["a", "b"], "{path:?}");
    }
}

#[test]
fn ranks_across_providers_together() {
    for path in PATHS {
        let mut pool = Pool::of(
            quota(SOONEST, 0),
            [
                claude("a", 10, 120),
                codex("x", 70, 30),
                claude("b", 20, 45),
            ],
        );
        assert_eq!(
            pool.order_across(path, &["claude", "codex"]),
            ["x", "b", "a"],
            "{path:?}"
        );
    }
}

#[test]
fn an_established_binding_wins() {
    let mut pool = Pool::of(
        quota(MOST_LEFT, 0),
        [claude("a", 10, 60), claude("b", 50, 60)],
    );
    let first = session("11111111-1111-4111-8111-111111111111");
    assert_eq!(pool.sticky(&first), "a");
    // `b` now has the most left; the bound session stays on `a`, a new
    // session takes `b`.
    pool.put(claude("a", 90, 60));
    assert_eq!(pool.sticky(&first), "a");
    let second = session("22222222-2222-4222-8222-222222222222");
    assert_eq!(pool.sticky(&second), "b");
}

/// Codex quota headers saying the primary window is `used` percent used and
/// resets an hour after `now`.
fn codex_headers(used: &str, now: Timestamp) -> http::HeaderMap {
    let mut headers = http::HeaderMap::new();
    let reset = (now + TimeDelta::hours(1)).timestamp().to_string();
    for (name, value) in [
        ("x-codex-primary-used-percent", used.to_owned()),
        ("x-codex-primary-reset-at", reset),
    ] {
        headers.insert(
            http::HeaderName::from_static(name),
            http::HeaderValue::from_str(&value).expect("header value"),
        );
    }
    headers
}

// The manager routes by the readings it records from each response.
#[tokio::test(start_paused = true)]
async fn the_manager_routes_by_recorded_readings() {
    let settings = Settings {
        routing_strategy: quota(SOONEST, 10),
        ..Settings::default()
    };
    let h = Harness::new(settings);
    let now = h.now();
    let executor = FakeExecutor::with("codex", move |call: &Call| {
        let used = if call.auth_id == "quota-a" {
            "95"
        } else {
            "50"
        };
        Reply::Ok(crate::exec::Response {
            payload: bytes::Bytes::from_static(b"{}"),
            headers: codex_headers(used, now),
        })
    });
    h.executor(&executor);
    for id in ["quota-a", "quota-b"] {
        h.add(auth(id, "codex"), &["gpt-quota"]);
    }
    for _ in 0..4 {
        h.manager
            .execute(&providers(&["codex"]), request("gpt-quota"), options())
            .await
            .expect("call");
    }
    // Neither has a reading at first, so `a` goes first; it then has no
    // room above the reserve, and every later call goes to `b`.
    assert_eq!(
        executor.ids(Kind::Execute),
        ["quota-a", "quota-b", "quota-b", "quota-b"]
    );
}
