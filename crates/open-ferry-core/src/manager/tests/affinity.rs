// Ported from CLIProxyAPI sdk/cliproxy/auth/selector_test.go,
// selector_lcp_test.go, selector_subagent_affinity_test.go,
// session_affinity_priority_test.go, session_affinity_metadata_test.go and
// conductor_session_affinity_alias_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Picking with session affinity: a session keeps the credential it is
//! bound to while that one is ready, whatever its priority, moves when it
//! isn't, and takes its parent's or its conversation's credential when it
//! has none of its own.
//!
//! Upstream calls its `SessionAffinitySelector` on a slice of credentials.
//! Here a [`Sticky`] pool holds the credentials and picks among the ones a
//! test names through `Selection::pick_sticky`, the affinity pick with the
//! routing strategy as its fallback. The manager tests run whole calls.
//!
//! Deviations from upstream:
//! - The provider scope and the session are arguments: upstream reads the
//!   session from the call's options and writes the canonical session, its
//!   parent and the fork flag into the call's metadata, where these tests
//!   check the session's `primary`, `fallback` and `is_fork`. A result
//!   names its model, where upstream reads the one its pick wrote.
//! - A session derived upstream comes from the call's metadata; here
//!   `Session::with_derived` takes it. The caller scope only keeps derived
//!   sessions apart, so tests that set it with an explicit session leave it
//!   out.
//! - The Antigravity scopes and credentials are Gemini's. Not ported:
//!   `SessionAffinityAntigravitySubagentInheritsParentByDefault` and the
//!   Antigravity session header case of
//!   `SessionAffinityExtendedHeadersAndPayloadIdentities` (Antigravity is
//!   out of scope).
//! - `SessionAffinityFallbackOnlyReceivesHighestAvailablePriority` falls
//!   back to fill first where upstream's selector takes the last credential,
//!   so the credentials' IDs are swapped to keep the lower tier first.
//! - `SessionAffinitySelectorPrimaryTrafficKeepsConversationAliasAlive`
//!   moves the time past the conversation's first TTL where upstream
//!   expires its entry by hand, as a session's keys share one expiry.
//! - `SessionAffinitySelector_Concurrent` runs 32 Tokio tasks of 50 manager
//!   calls each, since the bindings live in the manager.
//! - `ManagerSessionAffinityAliasCooldownPreservesSelection` picks its
//!   "select" path through `pick_next_mixed_sticky`, as `SelectAuth` isn't
//!   ported; its "lcp" mode isn't ported, and the alias's `Fork: true` is
//!   dropped as in `conductor_alias_cooldown`.
//! - `ManagerSessionAffinityPreservesBindingAcrossHigherPriorityRecovery`:
//!   calls only pick from one provider through the credential policy's pick
//!   (upstream's `pickNextLegacy`), so the single provider case picks for
//!   Codex Alpha Search, among Codex sign-ins.
//! - Not ported: `SessionAffinitySelectorNilFallbackNoPanic` (there is no
//!   nil fallback), the LCP tests and
//!   `SessionAffinitySelectorExplicitHarnessSessionOverridesLCP` (the LCP
//!   matcher isn't ported), `CanonicalSessionIDUnifiedResolution` (nor is
//!   `CanonicalSessionID`), `SessionAffinitySelectorLookupAffinityProviderAlias`
//!   (only the plugin host calls `LookupAffinity`) and the
//!   `ManagerSetSelector*` tests (the strategy is a setting, not a
//!   pluggable selector).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeDelta, TimeZone, Utc};
use http::{HeaderMap, HeaderName, HeaderValue};

use super::support::*;
use crate::auth::{Auth, AuthError, Status, Timestamp};
use crate::exec::{Dispatcher, ErrorKind, ExecError};
use crate::executor::ProviderExecutor;
use crate::manager::affinity::{Affinity, Session};
use crate::manager::models::{OAuthAliasTable, Resolver};
use crate::manager::policy::CredentialPolicy;
use crate::manager::select::{PickArgs, Selection, SelectorState};
use crate::manager::{CallResult, Entry, ModelAlias, RoutingStrategy, Settings};
use crate::session::Payload;

const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(60 * 60);

fn base_now() -> Timestamp {
    Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
        .single()
        .expect("valid time")
}

/// Headers holding each of `pairs`, in order.
pub(super) fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.append(
            HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            HeaderValue::from_str(value).expect("header value"),
        );
    }
    map
}

/// The session of a call with headers `pairs` and body `body`, if it has
/// one.
fn try_session(pairs: &[(&str, &str)], body: &str) -> Option<Session> {
    Session::with_derived(&headers(pairs), &Payload::parse(body.as_bytes()), "", "")
}

/// The session of a call with headers `pairs` and body `body`.
pub(super) fn session(pairs: &[(&str, &str)], body: &str) -> Session {
    try_session(pairs, body).expect("a session")
}

/// The session of a call with body `body` alone.
fn body(body: &str) -> Session {
    session(&[], body)
}

/// The session of a call whose metadata holds only the derived session `id`.
fn derived(id: &str) -> Session {
    Session::with_derived(&HeaderMap::new(), &Payload::parse(b""), "", id).expect("a session")
}

/// A credential with `id`.
fn cred(id: &str) -> Auth {
    Auth {
        id: id.to_owned(),
        provider: "test".to_owned(),
        status: Status::Active,
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

/// A failed call's outcome on `auth` for `model`, with no error.
fn failed(auth: &str, model: &str) -> CallResult {
    CallResult {
        auth_id: auth.to_owned(),
        model: model.to_owned(),
        ..CallResult::default()
    }
}

/// Credentials, the rotation state picks keep, and the bindings.
pub(super) struct Sticky {
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

impl Sticky {
    /// A pool picking by `strategy`, whose bindings live a minute and pass
    /// to subagents.
    fn new(strategy: RoutingStrategy) -> Self {
        Self::with(strategy, Affinity::new(MINUTE, true))
    }

    fn with(strategy: RoutingStrategy, affinity: Affinity) -> Self {
        Self {
            strategy,
            auths: BTreeMap::new(),
            executors: HashMap::new(),
            models: FakeModels::default(),
            settings: Settings::default(),
            oauth: OAuthAliasTable::default(),
            state: SelectorState::default(),
            affinity,
            now: base_now(),
        }
    }

    /// A pool picking by `strategy` with credentials `ids`.
    pub(super) fn of(strategy: RoutingStrategy, ids: &[&str]) -> Self {
        let mut pool = Self::new(strategy);
        for id in ids {
            pool.put(cred(id));
        }
        pool
    }

    /// Adds `auth`, or replaces the one with its ID.
    fn put(&mut self, auth: Auth) {
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

    /// Picks among the credentials `ids`, in that order, for a call of
    /// `session` in `scope` for `model`.
    fn try_pick(
        &mut self,
        scope: &str,
        model: &str,
        session: Option<&Session>,
        ids: &[&str],
    ) -> Result<String, ExecError> {
        let candidates: Vec<&Arc<Auth>> = ids.iter().map(|id| &self.auths[*id].auth).collect();
        let selection = Selection {
            auths: &self.auths,
            executors: &self.executors,
            models: &self.models,
            resolver: Resolver {
                settings: &self.settings,
                oauth: &self.oauth,
            },
            strategy: self.strategy,
            now: self.now,
        };
        selection
            .pick_sticky(
                &mut self.state,
                &mut self.affinity,
                &candidates,
                scope,
                model,
                session,
            )
            .map(|auth| auth.id.clone())
    }

    pub(super) fn pick(
        &mut self,
        scope: &str,
        model: &str,
        session: &Session,
        ids: &[&str],
    ) -> String {
        self.try_pick(scope, model, Some(session), ids)
            .unwrap_or_else(|err| panic!("pick: {err}"))
    }

    /// Records `result` for `session` in `scope`.
    pub(super) fn on_result(&mut self, session: &Session, result: &CallResult, scope: &str) {
        self.affinity.on_result(session, result, scope, self.now);
    }

    /// The credential `key` is bound to.
    fn bound(&mut self, key: &str) -> Option<String> {
        self.affinity.bound(key, self.now)
    }
}

const ABC: [&str; 3] = ["auth-a", "auth-b", "auth-c"];
const AB: [&str; 2] = ["auth-a", "auth-b"];

// TestSessionAffinitySelector_SameSessionSameAuth.
#[test]
fn same_session_same_auth() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &ABC);
    let session = body(
        r#"{"metadata":{"user_id":"user_xxx_account__session_ac980658-63bd-4fb3-97ba-8da64cb1e344"}}"#,
    );
    let first = pool.pick("claude", "claude-3", &session, &ABC);
    for i in 0..10 {
        assert_eq!(
            pool.pick("claude", "claude-3", &session, &ABC),
            first,
            "pick #{i}"
        );
    }
}

// TestSessionAffinitySelector_ThinkingSuffixVariantsPreserveBindingAndRelease.
#[test]
fn thinking_suffix_variants_preserve_binding_and_release() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &ABC);
    let session = body(
        r#"{"metadata":{"user_id":"user_xxx_account__session_ac980658-63bd-4fb3-97ba-8da64cb1e344"}}"#,
    );
    let first = pool.pick("anthropic", "claude-sonnet-4-5", &session, &ABC);
    for variant in ["claude-sonnet-4-5(high)", "claude-sonnet-4-5(medium)"] {
        assert_eq!(
            pool.pick("anthropic", variant, &session, &ABC),
            first,
            "{variant} should keep the session's credential"
        );
    }

    // A failure on a suffix variant releases the binding.
    let result = CallResult {
        provider: "anthropic".into(),
        error: Some(AuthError {
            code: "rate_limited".into(),
            message: "rate limited".into(),
            ..AuthError::default()
        }),
        ..failed(&first, "claude-sonnet-4-5(high)")
    };
    pool.on_result(&session, &result, "anthropic");

    let next = pool.pick("anthropic", "claude-sonnet-4-5", &session, &ABC);
    assert_ne!(next, first, "the released session should pick again");
}

// TestSessionAffinitySelector_WeightedBindingRebindsAfterWeightBecomesZero.
#[test]
fn weighted_binding_rebinds_after_weight_becomes_zero() {
    let mut pool = Sticky::new(RoutingStrategy::Weighted);
    pool.put(weighted("auth-a", "1"));
    pool.put(weighted("auth-b", "1"));
    let session = body(r#"{"metadata":{"user_id":"user_xxx_account__session_weight-change"}}"#);

    assert_eq!(pool.pick("claude", "claude-3", &session, &AB), "auth-a");
    pool.put(weighted("auth-a", "0"));
    assert_eq!(pool.pick("claude", "claude-3", &session, &AB), "auth-b");
    pool.put(weighted("auth-a", "10"));
    assert_eq!(
        pool.pick("claude", "claude-3", &session, &AB),
        "auth-b",
        "the rebound session should stay"
    );
}

// Not upstream's: the weighted affinity pick with zero-weight credentials
// ready, as a Go probe of v8.0.15's `pickNext` gave, with a session and
// without. Zero weights are left out of every tier before the pick, so a
// lower tier's positive weight serves; with none ready there is no
// candidate, unless every credential is cooling down.
#[test]
fn weighted_pick_with_ready_zero_weight_credentials() {
    let model = "probe-model";
    let auth = |id: &str, weight: &str, priority: &str| {
        with_attr(weighted(id, weight), "priority", priority)
    };
    let cooling = |auth: Auth| {
        let next = base_now() + TimeDelta::minutes(10);
        let mut state = crate::auth::ModelState {
            status: Status::Error,
            unavailable: true,
            next_retry_after: Some(next),
            ..Default::default()
        };
        state.quota.exceeded = true;
        state.quota.next_recover_at = Some(next);
        let mut auth = auth;
        auth.model_states.insert(model.to_owned(), state);
        auth
    };
    let cases = [
        (
            "positive weights cooling down",
            vec![
                cooling(auth("p1", "1", "0")),
                cooling(auth("p2", "1", "0")),
                auth("z", "0", "0"),
            ],
            Err(ErrorKind::AuthNotFound),
        ),
        (
            "zero weight alone in the higher tier",
            vec![auth("p", "1", "0"), auth("z", "0", "10")],
            Ok("p"),
        ),
        (
            "every credential cooling down",
            vec![cooling(auth("p1", "1", "0")), cooling(auth("z", "0", "0"))],
            Err(ErrorKind::ModelCooldown),
        ),
        (
            "zero weight the only ready one in the higher tier",
            vec![
                cooling(auth("p1", "1", "10")),
                auth("z", "0", "10"),
                auth("p2", "1", "0"),
            ],
            Ok("p2"),
        ),
    ];
    for (name, auths, want) in cases {
        let ids: Vec<String> = auths.iter().map(|a| a.id.clone()).collect();
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        for session in [None, Some(derived("zero-weight"))] {
            let mut pool = Sticky::new(RoutingStrategy::Weighted);
            for auth in auths.clone() {
                pool.put(auth);
            }
            let got = pool.try_pick("gemini", model, session.as_ref(), &ids);
            let label = format!("{name}, session {}", session.is_some());
            match want {
                Ok(id) => assert_eq!(got.ok().as_deref(), Some(id), "{label}"),
                Err(kind) => {
                    let err = got.expect_err(&label);
                    assert_eq!(err.kind, kind, "{label}: {err}");
                }
            }
        }
    }
}

// TestSessionAffinitySelector_WeightedNewSessionsResetAfterWeightChange.
#[test]
fn weighted_new_sessions_reset_after_weight_change() {
    let mut pool = Sticky::new(RoutingStrategy::Weighted);
    pool.put(weighted("auth-a", "1000000"));
    pool.put(weighted("auth-b", "1"));
    let pick_session = |pool: &mut Sticky, index: usize| {
        let session = body(&format!(r#"{{"session_id":"session-{index}"}}"#));
        pool.pick("claude", "claude-3", &session, &AB)
    };
    for index in 0..1000 {
        pick_session(&mut pool, index);
    }

    pool.put(weighted("auth-a", "1"));
    let mut counts: HashMap<String, usize> = HashMap::new();
    for index in 1000..1020 {
        *counts.entry(pick_session(&mut pool, index)).or_default() += 1;
    }
    assert_eq!(
        (counts.get("auth-a"), counts.get("auth-b")),
        (Some(&10), Some(&10)),
        "new session picks after the weight change: {counts:?}"
    );
}

// TestSessionAffinitySelector_NoSessionFallback.
#[test]
fn no_session_fallback() {
    let mut pool = Sticky::of(RoutingStrategy::FillFirst, &ABC);
    let session = try_session(&[], r#"{"model":"claude-3"}"#);
    assert!(session.is_none(), "the call names no session");
    let got = pool
        .try_pick(
            "claude",
            "claude-3",
            session.as_ref(),
            &["auth-b", "auth-a", "auth-c"],
        )
        .expect("pick");
    assert_eq!(got, "auth-a", "should fall back to fill first");
}

// TestSessionAffinitySelector_DifferentSessionsDifferentAuths.
#[test]
fn different_sessions_different_auths() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &ABC);
    let session1 = body(
        r#"{"metadata":{"user_id":"user_xxx_account__session_11111111-1111-1111-1111-111111111111"}}"#,
    );
    let session2 = body(
        r#"{"metadata":{"user_id":"user_xxx_account__session_22222222-2222-2222-2222-222222222222"}}"#,
    );
    let auth1 = pool.pick("claude", "claude-3", &session1, &ABC);
    let auth2 = pool.pick("claude", "claude-3", &session2, &ABC);
    for i in 0..5 {
        assert_eq!(
            pool.pick("claude", "claude-3", &session1, &ABC),
            auth1,
            "session 1 #{i}"
        );
        assert_eq!(
            pool.pick("claude", "claude-3", &session2, &ABC),
            auth2,
            "session 2 #{i}"
        );
    }
}

// TestSessionAffinitySelector_FailoverWhenAuthUnavailable.
#[test]
fn failover_when_auth_unavailable() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &ABC);
    let session =
        body(r#"{"metadata":{"user_id":"user_xxx_account__session_failover-test-uuid"}}"#);
    let first = pool.pick("claude", "claude-3", &session, &ABC);

    // The bound credential drops out, as on a rate limit.
    let rest: Vec<&str> = ABC.into_iter().filter(|id| *id != first).collect();
    let second = pool.pick("claude", "claude-3", &session, &rest);
    assert_ne!(second, first, "failover kept the unavailable credential");
    for i in 0..5 {
        assert_eq!(
            pool.pick("claude", "claude-3", &session, &rest),
            second,
            "pick #{i} after failover"
        );
    }
}

// TestSessionAffinitySelector_ThreeScenarios.
#[test]
fn three_scenarios() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &ABC);
    let cases = [
        (
            "OpenAI_Scenario1_NewRequest",
            r#"{"messages":[{"role":"system","content":"You are helpful"},{"role":"user","content":"Hello"}]}"#,
        ),
        (
            "OpenAI_Scenario2_SecondTurn",
            r#"{"messages":[{"role":"system","content":"You are helpful"},{"role":"user","content":"Hello"},{"role":"assistant","content":"Hi there!"},{"role":"user","content":"Help me"}]}"#,
        ),
        (
            "OpenAI_Scenario3_ManyTurns",
            r#"{"messages":[{"role":"system","content":"You are helpful"},{"role":"user","content":"Hello"},{"role":"assistant","content":"Hi there!"},{"role":"user","content":"Help me"},{"role":"assistant","content":"Sure!"},{"role":"user","content":"Thanks"}]}"#,
        ),
        (
            "Gemini_Scenario1_NewRequest",
            r#"{"systemInstruction":{"parts":[{"text":"You are helpful"}]},"contents":[{"role":"user","parts":[{"text":"Hello Gemini"}]}]}"#,
        ),
        (
            "Gemini_Scenario2_SecondTurn",
            r#"{"systemInstruction":{"parts":[{"text":"You are helpful"}]},"contents":[{"role":"user","parts":[{"text":"Hello Gemini"}]},{"role":"model","parts":[{"text":"Hi!"}]},{"role":"user","parts":[{"text":"Help"}]}]}"#,
        ),
        (
            "Gemini_Scenario3_ManyTurns",
            r#"{"systemInstruction":{"parts":[{"text":"You are helpful"}]},"contents":[{"role":"user","parts":[{"text":"Hello Gemini"}]},{"role":"model","parts":[{"text":"Hi!"}]},{"role":"user","parts":[{"text":"Help"}]},{"role":"model","parts":[{"text":"Sure!"}]},{"role":"user","parts":[{"text":"Thanks"}]}]}"#,
        ),
        (
            "Claude_Scenario1_NewRequest",
            r#"{"messages":[{"role":"user","content":"Hello Claude"}]}"#,
        ),
        (
            "Claude_Scenario2_SecondTurn",
            r#"{"messages":[{"role":"user","content":"Hello Claude"},{"role":"assistant","content":"Hello!"},{"role":"user","content":"Help me"}]}"#,
        ),
        (
            "Claude_Scenario3_ManyTurns",
            r#"{"messages":[{"role":"user","content":"Hello Claude"},{"role":"assistant","content":"Hello!"},{"role":"user","content":"Help"},{"role":"assistant","content":"Sure!"},{"role":"user","content":"Thanks"}]}"#,
        ),
    ];
    for (name, payload) in cases {
        pool.try_pick("provider", "model", Some(&body(payload)), &ABC)
            .unwrap_or_else(|err| panic!("{name}: {err}"));
    }

    // Scenario2And3_SameAuth.
    let second = body(
        r#"{"messages":[{"role":"system","content":"Stable test"},{"role":"user","content":"First msg"},{"role":"assistant","content":"Response"},{"role":"user","content":"Second"}]}"#,
    );
    let third = body(
        r#"{"messages":[{"role":"system","content":"Stable test"},{"role":"user","content":"First msg"},{"role":"assistant","content":"Response"},{"role":"user","content":"Second"},{"role":"assistant","content":"More"},{"role":"user","content":"Third"}]}"#,
    );
    let picked2 = pool.pick("test", "model", &second, &ABC);
    let picked3 = pool.pick("test", "model", &third, &ABC);
    assert_eq!(picked2, picked3, "scenarios 2 and 3 should pick the same");

    // Scenario1To2_InheritBinding.
    let first = body(
        r#"{"messages":[{"role":"system","content":"Inherit test"},{"role":"user","content":"Initial"}]}"#,
    );
    let second = body(
        r#"{"messages":[{"role":"system","content":"Inherit test"},{"role":"user","content":"Initial"},{"role":"assistant","content":"Reply"},{"role":"user","content":"Continue"}]}"#,
    );
    let picked1 = pool.pick("inherit", "model", &first, &ABC);
    let picked2 = pool.pick("inherit", "model", &second, &ABC);
    assert_eq!(picked1, picked2, "scenario 2 should inherit scenario 1");
}

const BOTH: &str =
    r#"{"conversation":{"id":"conversation-session"},"prompt_cache_key":"shared-cache-bucket"}"#;
const PROMPT_ONLY: &str = r#"{"prompt_cache_key":"shared-cache-bucket"}"#;
const CONVERSATION_ONLY: &str = r#"{"conversation":{"id":"conversation-session"}}"#;

// TestSessionAffinitySelectorBodyIdentifierTransitionsPreserveBinding.
#[test]
fn body_identifier_transitions_preserve_binding() {
    let both = body(BOTH);
    assert_eq!(
        (both.primary(), both.fallback()),
        ("pck:shared-cache-bucket", "conv:conversation-session"),
        "want the prompt cache key with the conversation as its fallback"
    );
    for (name, first) in [
        ("prompt cache first", PROMPT_ONLY),
        ("conversation first", CONVERSATION_ONLY),
    ] {
        let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &AB);
        let scope = format!("responses-transition-{name}");
        let first = pool.pick(&scope, "gpt-test", &body(first), &AB);
        let second = pool.pick(&scope, "gpt-test", &both, &AB);
        assert_eq!(second, first, "{name}: combined identifiers moved");
    }
}

// TestSessionAffinitySelectorCombinedIdentifiersBindConversationFallback.
#[test]
fn combined_identifiers_bind_conversation_fallback() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &AB);
    let scope = "responses-combined-to-conversation";
    let first = pool.pick(scope, "gpt-test", &body(BOTH), &AB);
    let second = pool.pick(scope, "gpt-test", &body(CONVERSATION_ONLY), &AB);
    assert_eq!(second, first, "dropping prompt_cache_key moved the session");
}

// TestSessionAffinitySelectorPrimaryTrafficKeepsConversationAliasAlive,
// with the time moved past the conversation's first TTL.
#[test]
fn primary_traffic_keeps_conversation_alias_alive() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &AB);
    let scope = "responses-active-primary-alias";
    let first = pool.pick(scope, "gpt-test", &body(BOTH), &AB);

    pool.now = base_now() + TimeDelta::seconds(50);
    let primary = pool.pick(scope, "gpt-test", &body(PROMPT_ONLY), &AB);
    assert_eq!(primary, first, "prompt-only");

    pool.now = base_now() + TimeDelta::seconds(100);
    let fallback = pool.pick(scope, "gpt-test", &body(CONVERSATION_ONLY), &AB);
    assert_eq!(
        fallback, first,
        "the conversation expired while its prompt cache key was in use"
    );
}

// TestSessionAffinitySelectorSharedPromptKeyPreservesConversationAliases.
#[test]
fn shared_prompt_key_preserves_conversation_aliases() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &AB);
    let scope = "responses-shared-prompt-key";
    let combined_a =
        r#"{"conversation":{"id":"conversation-a"},"prompt_cache_key":"shared-cache-bucket"}"#;
    let combined_b =
        r#"{"conversation":{"id":"conversation-b"},"prompt_cache_key":"shared-cache-bucket"}"#;
    let first = pool.pick(scope, "gpt-test", &body(combined_a), &AB);
    let second = pool.pick(scope, "gpt-test", &body(combined_b), &AB);
    assert_eq!(second, first, "the shared prompt key moved");
    for (name, payload) in [
        (
            "conversation A",
            r#"{"conversation":{"id":"conversation-a"}}"#,
        ),
        (
            "conversation B",
            r#"{"conversation":{"id":"conversation-b"}}"#,
        ),
    ] {
        assert_eq!(
            pool.pick(scope, "gpt-test", &body(payload), &AB),
            first,
            "{name}"
        );
    }
}

// TestSessionAffinitySelectorConversationIDContainingPromptMarkerRemainsStable.
#[test]
fn conversation_id_containing_prompt_marker_remains_stable() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &AB);
    let scope = "responses-opaque-conversation";
    let combined = r#"{"conversation":{"id":"a::pck:b"},"prompt_cache_key":"shared-cache-bucket"}"#;
    let first = pool.pick(scope, "gpt-test", &body(combined), &AB);
    let second = pool.pick(
        scope,
        "gpt-test",
        &body(r#"{"conversation":{"id":"a::pck:b"}}"#),
        &AB,
    );
    assert_eq!(second, first);
}

// TestSessionAffinitySelector_MultiModelSession.
#[test]
fn multi_model_session() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &AB);
    let session = body(r#"{"metadata":{"user_id":"user_xxx_account__session_multi-model-test"}}"#);
    // auth-a serves only model-a, auth-b only model-b.
    assert_eq!(
        pool.pick("provider", "model-a", &session, &["auth-a"]),
        "auth-a"
    );
    assert_eq!(
        pool.pick("provider", "model-b", &session, &["auth-b"]),
        "auth-b"
    );
    assert_eq!(
        pool.pick("provider", "model-a", &session, &["auth-a"]),
        "auth-a",
        "each model keeps its own binding"
    );
    for i in 0..5 {
        assert_eq!(
            pool.pick("provider", "model-a", &session, &["auth-a"]),
            "auth-a",
            "#{i}"
        );
        assert_eq!(
            pool.pick("provider", "model-b", &session, &["auth-b"]),
            "auth-b",
            "#{i}"
        );
    }
}

// TestSessionAffinitySelector_CrossProviderIsolation.
#[test]
fn cross_provider_isolation() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &["auth-claude", "auth-gemini"]);
    let session =
        body(r#"{"metadata":{"user_id":"user_xxx_account__session_cross-provider-test"}}"#);
    let claude = ["auth-claude"];
    let gemini = ["auth-gemini"];
    assert_eq!(
        pool.pick("claude", "claude-3", &session, &claude),
        "auth-claude"
    );
    assert_eq!(
        pool.pick("gemini", "gemini-2.5-pro", &session, &gemini),
        "auth-gemini"
    );
    for i in 0..5 {
        assert_eq!(
            pool.pick("claude", "claude-3", &session, &claude),
            "auth-claude",
            "#{i}"
        );
        assert_eq!(
            pool.pick("gemini", "gemini-2.5-pro", &session, &gemini),
            "auth-gemini",
            "#{i}"
        );
    }
}

// TestSessionAffinitySelector_RoundRobinDistribution.
#[test]
fn round_robin_distribution() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &ABC);
    let mut counts: HashMap<String, usize> = HashMap::new();
    for i in 0..12 {
        let session = body(&format!(
            r#"{{"metadata":{{"user_id":"user_xxx_account__session_{i:08}-0000-0000-0000-000000000000"}}}}"#
        ));
        *counts
            .entry(pool.pick("provider", "model", &session, &ABC))
            .or_default() += 1;
    }
    for id in ABC {
        assert_eq!(counts.get(id), Some(&4), "{id}: {counts:?}");
    }
}

// TestSessionAffinitySelector_DerivedIDBoundConsistencyOnResult.
#[test]
fn derived_id_bound_consistency_on_result() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &AB);
    let session = derived(&"d".repeat(250));
    let picked = pool.pick("openai", "gpt-5.4", &session, &AB);
    let success = CallResult {
        provider: "openai".into(),
        success: true,
        ..failed(&picked, "gpt-5.4")
    };
    pool.on_result(&session, &success, "openai");

    let second = pool.pick("openai", "gpt-5.4", &session, &["auth-b", "auth-a"]);
    assert_eq!(second, picked);
}

// TestExtractExplicitSessionIDs_EnhancedHarnesses, part 3: a Roo Code
// subagent task takes its parent task's credential.
#[test]
fn enhanced_harness_subagent_task_inherits_parent() {
    let ids = ["auth-1", "auth-2"];
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &ids);
    let parent = session(&[("X-Task-ID", "task-parent-100")], "");
    let parent_auth = pool.pick("openai", "gpt-5.4", &parent, &ids);
    let success = CallResult {
        provider: "openai".into(),
        success: true,
        ..failed(&parent_auth, "gpt-5.4")
    };
    pool.on_result(&parent, &success, "openai");

    let child = session(
        &[
            ("X-Task-ID", "task-child-101"),
            ("X-Parent-Task-ID", "task-parent-100"),
        ],
        "",
    );
    let child_auth = pool.pick("openai", "gpt-5.4", &child, &["auth-2", "auth-1"]);
    assert_eq!(child_auth, parent_auth, "the subagent task moved");
}

// TestSessionAffinityFallbackOnlyReceivesHighestAvailablePriority, with
// fill first and the IDs swapped.
#[test]
fn fallback_only_receives_highest_available_priority() {
    let mut pool = Sticky::with(RoutingStrategy::FillFirst, Affinity::new(HOUR, true));
    pool.put(with_priority("z-high", "1"));
    pool.put(with_priority("a-low", "0"));
    let ids = ["z-high", "a-low"];
    let session = derived("stable-session");

    assert_eq!(
        pool.pick("test", "model", &session, &ids),
        "z-high",
        "cold binding"
    );
    assert_eq!(
        pool.try_pick("test", "model", None, &ids).expect("pick"),
        "z-high",
        "no-session fallback"
    );

    pool.put(Auth {
        unavailable: true,
        ..with_priority("z-high", "1")
    });
    assert_eq!(
        pool.pick("test", "model", &session, &ids),
        "a-low",
        "fallback after the bound credential became unavailable"
    );
}

// TestSessionAffinityGeminiSubagentInheritsParentWhenExplicitlyTrue.
#[test]
fn gemini_subagent_inherits_parent_when_explicitly_true() {
    let ids = ["auth-gem-1", "auth-gem-2"];
    let mut pool = Sticky::with(RoutingStrategy::RoundRobin, Affinity::new(MINUTE, true));
    for id in ids {
        pool.put(cred(id));
    }
    let parent = session(
        &[("X-Claude-Code-Session-Id", "gem-root-200")],
        r#"{"messages":[{"role":"user","content":"parent gemini task"}]}"#,
    );
    let parent_auth = pool.pick("gemini", "gemini-2.5-pro", &parent, &ids);
    assert_eq!(parent_auth, "auth-gem-1");

    let subagent = session(
        &[
            ("X-Claude-Code-Session-Id", "gem-root-200"),
            ("X-Claude-Code-Agent-Id", "gem-sub-001"),
        ],
        r#"{"messages":[{"role":"user","content":"subagent gemini task"}]}"#,
    );
    assert_eq!(
        pool.pick("gemini", "gemini-2.5-pro", &subagent, &ids),
        parent_auth
    );
}

// TestSessionAffinitySubagentIsolatesWhenExplicitlyFalse.
#[test]
fn subagent_isolates_when_explicitly_false() {
    let ids = ["auth-1", "auth-2"];
    let mut pool = Sticky::with(RoutingStrategy::RoundRobin, Affinity::new(MINUTE, false));
    for id in ids {
        pool.put(cred(id));
    }
    let parent = session(
        &[("X-Claude-Code-Session-Id", "sess-iso-300")],
        r#"{"messages":[{"role":"user","content":"parent task"}]}"#,
    );
    assert_eq!(
        pool.pick("claude", "claude-3-7-sonnet", &parent, &ids),
        "auth-1"
    );

    let subagent = session(
        &[
            ("X-Claude-Code-Session-Id", "sess-iso-300"),
            ("X-Claude-Code-Agent-Id", "sub-iso-001"),
        ],
        r#"{"messages":[{"role":"user","content":"subagent task"}]}"#,
    );
    assert_eq!(
        pool.pick("claude", "claude-3-7-sonnet", &subagent, &ids),
        "auth-2",
        "the subagent took its parent's credential"
    );
}

const FLASH: &str = "gemini-3.7-flash-high";

// TestSessionAffinitySubagentFailureIsolation, in the Gemini scope.
#[test]
fn subagent_failure_isolation() {
    let ids = ["auth-1", "auth-2"];
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &ids);
    let parent = session(
        &[("X-Claude-Code-Session-Id", "sess-fail-400")],
        r#"{"messages":[{"role":"user","content":"parent task"}]}"#,
    );
    assert_eq!(pool.pick("gemini", FLASH, &parent, &ids), "auth-1");
    let parent_key = "gemini::claude:sess-fail-400::gemini-3.7-flash-high";
    assert_eq!(pool.bound(parent_key).as_deref(), Some("auth-1"));

    let subagent = session(
        &[
            ("X-Claude-Code-Session-Id", "sess-fail-400"),
            ("X-Claude-Code-Agent-Id", "sub-fail-001"),
        ],
        r#"{"messages":[{"role":"user","content":"subagent task"}]}"#,
    );
    assert_eq!(pool.pick("gemini", FLASH, &subagent, &ids), "auth-1");
    let subagent_key = "gemini::claude:sess-fail-400:agent:sub-fail-001::gemini-3.7-flash-high";
    assert_eq!(pool.bound(subagent_key).as_deref(), Some("auth-1"));

    pool.on_result(&subagent, &failed("auth-1", FLASH), "gemini");
    assert_eq!(pool.bound(subagent_key), None, "the subagent stayed bound");
    assert_eq!(
        pool.bound(parent_key).as_deref(),
        Some("auth-1"),
        "the subagent's failure unbound its parent"
    );
    assert_eq!(pool.pick("gemini", FLASH, &parent, &ids), "auth-1");
}

// TestSessionAffinitySubagentAliasIsolation, in the Gemini scope.
#[test]
fn subagent_alias_isolation() {
    let ids = ["auth-1", "auth-2"];
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &ids);
    let parent = session(
        &[("X-Claude-Code-Session-Id", "sess-alias-500")],
        r#"{"messages":[{"role":"user","content":"parent task"}]}"#,
    );
    pool.pick("gemini", FLASH, &parent, &ids);
    let subagent = session(
        &[
            ("X-Claude-Code-Session-Id", "sess-alias-500"),
            ("X-Claude-Code-Agent-Id", "sub-alias-001"),
        ],
        r#"{"messages":[{"role":"user","content":"sub task"}]}"#,
    );
    pool.pick("gemini", FLASH, &subagent, &ids);

    pool.on_result(&subagent, &failed("auth-1", FLASH), "gemini");
    assert_eq!(
        pool.bound("gemini::claude:sess-alias-500::gemini-3.7-flash-high")
            .as_deref(),
        Some("auth-1"),
        "the subagent bound its parent's key with its own"
    );
}

// TestSessionAffinityClaudeSubagentInheritsParentBindingAndSeparatesAgentID.
#[test]
fn claude_subagent_inherits_parent_binding_and_separates_agent_id() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &AB);
    let parent = session(
        &[("X-Claude-Code-Session-Id", "claude-root-100")],
        r#"{"messages":[{"role":"user","content":"parent task"}]}"#,
    );
    let parent_auth = pool.pick("claude", "model", &parent, &AB);
    assert_eq!(parent.primary(), "claude:claude-root-100");

    for (agent, task) in [
        ("subagent-001", "subagent 1 task"),
        ("subagent-002", "subagent 2 task"),
    ] {
        let subagent = session(
            &[
                ("X-Claude-Code-Session-Id", "claude-root-100"),
                ("X-Claude-Code-Agent-Id", agent),
            ],
            &format!(r#"{{"messages":[{{"role":"user","content":"{task}"}}]}}"#),
        );
        assert_eq!(
            pool.pick("claude", "model", &subagent, &AB),
            parent_auth,
            "{agent} did not take its parent's credential"
        );
        assert_eq!(
            subagent.primary(),
            format!("claude:claude-root-100:agent:{agent}")
        );
    }
}

const FORK_TURN: &str = r#"{"session_id":"thread-fork-777","forked_from_thread_id":"thread-parent-999","request_kind":"turn"}"#;

// TestSessionAffinityCodexSubagentInheritsParentThread.
#[test]
fn codex_subagent_inherits_parent_thread() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &AB);
    let parent = session(
        &[("Session-Id", "thread-parent-999")],
        r#"{"input":[{"role":"user","content":"parent task"}]}"#,
    );
    let parent_auth = pool.pick("openai", "model", &parent, &AB);

    let child = session(
        &[
            ("Session-Id", "thread-child-888"),
            ("x-codex-parent-thread-id", "thread-parent-999"),
            ("x-openai-subagent", "true"),
        ],
        r#"{"input":[{"role":"user","content":"child subagent task"}]}"#,
    );
    assert_eq!(pool.pick("openai", "model", &child, &AB), parent_auth);
    assert_eq!(child.primary(), "codex:thread-child-888");

    let fork = session(
        &[
            ("Session-Id", "thread-fork-777"),
            ("X-Codex-Turn-Metadata", FORK_TURN),
        ],
        r#"{"input":[{"role":"user","content":"fork turn"}]}"#,
    );
    assert_eq!(pool.pick("openai", "model", &fork, &AB), parent_auth);
    assert_eq!(fork.primary(), "codex:thread-fork-777");
}

const GEMINI_FLASH: &str = "gemini-3.8-flash-high";

// TestSessionAffinityCodexForkInheritsParentOnGeminiModel.
#[test]
fn codex_fork_inherits_parent_on_gemini_model() {
    let ids = ["auth-1", "auth-2"];
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &ids);
    let parent = session(
        &[("Session-Id", "parent-gemini-thread")],
        r#"{"input":[{"role":"user","content":"parent query"}]}"#,
    );
    let parent_auth = pool.pick("mixed", GEMINI_FLASH, &parent, &ids);

    let fork = session(
        &[
            ("Session-Id", "fork-gemini-thread"),
            (
                "X-Codex-Turn-Metadata",
                r#"{"session_id":"fork-gemini-thread","forked_from_thread_id":"parent-gemini-thread","request_kind":"turn"}"#,
            ),
        ],
        r#"{"input":[{"role":"user","content":"fork query"}]}"#,
    );
    assert_eq!(
        pool.pick("mixed", GEMINI_FLASH, &fork, &ids),
        parent_auth,
        "the fork did not take its parent's credential"
    );
    assert_eq!(fork.primary(), "codex:fork-gemini-thread");
    assert_eq!(fork.fallback(), "codex:parent-gemini-thread");
    assert!(fork.is_fork());
}

// TestSessionAffinityCodexMultiAgentV2CollabSpawn.
#[test]
fn codex_multi_agent_v2_collab_spawn() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &AB);
    let parent = session(
        &[
            ("Session-Id", "parent-root-100"),
            ("Thread-Id", "parent-root-100"),
            (
                "X-Codex-Turn-Metadata",
                r#"{"session_id":"parent-root-100","thread_id":"parent-root-100","agent_name":"/root","request_kind":"turn"}"#,
            ),
        ],
        r#"{"input":[{"role":"user","content":"parent task"}]}"#,
    );
    let parent_auth = pool.pick("openai", "model", &parent, &AB);

    let child = session(
        &[
            ("Session-Id", "parent-root-100"),
            ("Thread-Id", "subagent-thread-200"),
            ("X-Codex-Parent-Thread-Id", "parent-root-100"),
            ("X-Openai-Subagent", "collab_spawn"),
            (
                "X-Codex-Turn-Metadata",
                r#"{"session_id":"parent-root-100","thread_id":"subagent-thread-200","agent_name":"/root/check_readme","parent_thread_id":"parent-root-100","subagent_kind":"thread_spawn"}"#,
            ),
        ],
        r#"{"input":[{"role":"user","content":"child check readme"}]}"#,
    );
    let child_auth = pool.pick("openai", "model", &child, &AB);
    assert_eq!(child_auth, parent_auth);
    assert_eq!(child.primary(), "codex:parent-root-100:agent:check_readme");
    assert_eq!(child.fallback(), "codex:parent-root-100");

    // The child's failure leaves its parent bound.
    pool.on_result(&child, &failed(&child_auth, "model"), "openai");
    let parent2 = session(
        &[
            ("Session-Id", "parent-root-100"),
            ("Thread-Id", "parent-root-100"),
        ],
        r#"{"input":[{"role":"user","content":"parent task 2"}]}"#,
    );
    assert_eq!(
        pool.pick("openai", "model", &parent2, &AB),
        parent_auth,
        "the subagent's failure unbound its parent"
    );
}

// TestSessionAffinityBodyOnlyCodexFork.
#[test]
fn body_only_codex_fork() {
    let ids = ["auth-1", "auth-2"];
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &ids);
    let parent =
        body(r#"{"thread_id":"body-parent-100","messages":[{"role":"user","content":"parent"}]}"#);
    let parent_auth = pool.pick("mixed", GEMINI_FLASH, &parent, &ids);
    let fork = body(
        r#"{"thread_id":"body-fork-200","forked_from_thread_id":"body-parent-100","messages":[{"role":"user","content":"fork"}]}"#,
    );
    assert_eq!(pool.pick("mixed", GEMINI_FLASH, &fork, &ids), parent_auth);
    assert!(fork.is_fork());
}

// TestSessionAffinityForkRebindDoesNotMutateParentBinding.
#[test]
fn fork_rebind_does_not_mutate_parent_binding() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &AB);
    let parent = session(
        &[("Session-Id", "parent-thread-alpha")],
        r#"{"input":[{"role":"user","content":"parent"}]}"#,
    );
    assert_eq!(pool.pick("openai", "model", &parent, &AB), "auth-a");

    let turn = r#"{"session_id":"fork-thread-beta","forked_from_thread_id":"parent-thread-alpha","request_kind":"turn"}"#;
    let fork = session(
        &[
            ("Session-Id", "fork-thread-beta"),
            ("X-Codex-Turn-Metadata", turn),
        ],
        r#"{"input":[{"role":"user","content":"fork"}]}"#,
    );
    assert_eq!(pool.pick("openai", "model", &fork, &AB), "auth-a");

    // The fork fails on auth-a and moves to auth-b.
    pool.on_result(&fork, &failed("auth-a", "model"), "openai");
    let fork_retry = session(
        &[
            ("Session-Id", "fork-thread-beta"),
            ("X-Codex-Turn-Metadata", turn),
        ],
        r#"{"input":[{"role":"user","content":"fork retry"}]}"#,
    );
    assert_eq!(
        pool.pick("openai", "model", &fork_retry, &["auth-b"]),
        "auth-b"
    );

    let parent2 = session(
        &[("Session-Id", "parent-thread-alpha")],
        r#"{"input":[{"role":"user","content":"parent turn 2"}]}"#,
    );
    assert_eq!(
        pool.pick("openai", "model", &parent2, &AB),
        "auth-a",
        "the fork's failover moved its parent"
    );
}

// TestSessionAffinityCodexSubagentWithOmittedThreadIdRetainsParent.
#[test]
fn codex_subagent_with_omitted_thread_id_retains_parent() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &AB);
    let parent = session(
        &[("Session-Id", "sess-main-999")],
        r#"{"input":[{"role":"user","content":"parent"}]}"#,
    );
    let parent_auth = pool.pick("openai", "model", &parent, &AB);

    let subagent = session(
        &[
            ("Session-Id", "sess-main-999"),
            ("X-Openai-Subagent", "collab_spawn"),
            (
                "X-Codex-Turn-Metadata",
                r#"{"session_id":"sess-main-999","agent_name":"/root/worker","subagent_kind":"thread_spawn"}"#,
            ),
        ],
        r#"{"input":[{"role":"user","content":"subagent"}]}"#,
    );
    assert_eq!(pool.pick("openai", "model", &subagent, &AB), parent_auth);
    assert_eq!(subagent.fallback(), "codex:sess-main-999");
    assert_eq!(subagent.primary(), "codex:sess-main-999:agent:worker");
}

// TestSessionAffinityPayloadParentSessionInheritance.
#[test]
fn payload_parent_session_inheritance() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &AB);
    let parent =
        body(r#"{"session_id":"parent-sess-001","messages":[{"role":"user","content":"parent"}]}"#);
    let parent_auth = pool.pick("openai", "model", &parent, &AB);
    let child = body(
        r#"{"session_id":"child-sess-002","parent_session_id":"parent-sess-001","messages":[{"role":"user","content":"child"}]}"#,
    );
    assert_eq!(pool.pick("openai", "model", &child, &AB), parent_auth);
    assert_eq!(child.primary(), "session:child-sess-002");
}

// TestSessionAffinityExtendedHeadersAndPayloadIdentities, without its
// Antigravity session header case.
#[test]
fn extended_headers_and_payload_identities() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &AB);
    let cases = [
        (
            "slot",
            "openai",
            session(
                &[("X-Slot-Session-Id", "pi-slot-789")],
                r#"{"messages":[{"role":"user","content":"pi slot task"}]}"#,
            ),
            "slot:pi-slot-789",
        ),
        (
            "gemini cache",
            "gemini",
            body(
                r#"{"cachedContent":"projects/123/locations/us-central1/cachedContents/456","contents":[{"role":"user","parts":[{"text":"query"}]}]}"#,
            ),
            "geminicache:projects/123/locations/us-central1/cachedContents/456",
        ),
        (
            "thread",
            "openai",
            session(
                &[("X-Thread-Id", "thread_abc123")],
                r#"{"messages":[{"role":"user","content":"run thread"}]}"#,
            ),
            "thread:thread_abc123",
        ),
    ];
    for (name, scope, session, want) in cases {
        pool.pick(scope, "model", &session, &AB);
        assert_eq!(session.primary(), want, "{name}");
    }

    // metadata.agent_id takes its session's credential.
    let parent =
        body(r#"{"session_id":"sess-main-555","messages":[{"role":"user","content":"main"}]}"#);
    let parent_auth = pool.pick("openai", "model", &parent, &AB);
    let subagent = body(
        r#"{"session_id":"sess-main-555","metadata":{"agent_id":"worker-agent-1"},"messages":[{"role":"user","content":"worker"}]}"#,
    );
    assert_eq!(pool.pick("openai", "model", &subagent, &AB), parent_auth);
    assert_eq!(
        subagent.primary(),
        "session:sess-main-555:agent:worker-agent-1"
    );
}

// TestSessionAffinitySelectorSubagentInheritance, without the caller scope.
#[test]
fn selector_subagent_inheritance() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &AB);
    let model = "claude-3-7-sonnet";
    let root = session(
        &[("X-Claude-Code-Session-Id", "tree-root-sess")],
        r#"{"messages":[{"role":"user","content":"start root task"}]}"#,
    );
    let root_auth = pool.pick("claude", model, &root, &AB);

    let sub1 = session(
        &[
            ("X-Claude-Code-Session-Id", "tree-root-sess"),
            ("X-Claude-Code-Agent-Id", "checker-agent"),
        ],
        r#"{"messages":[{"role":"user","content":"run checker"}]}"#,
    );
    assert_eq!(pool.pick("claude", model, &sub1, &AB), root_auth, "sub1");

    let sub2 = session(
        &[
            ("X-Claude-Code-Session-Id", "tree-root-sess"),
            ("X-Claude-Code-Agent-Id", "leaf-agent"),
            ("X-Claude-Code-Parent-Agent-Id", "checker-agent"),
        ],
        r#"{"messages":[{"role":"user","content":"run leaf checker"}]}"#,
    );
    assert_eq!(pool.pick("claude", model, &sub2, &AB), root_auth, "sub2");
}

// TestSessionAffinityClaudeMetadataSubagentNonInheritingGeminiModel.
#[test]
fn claude_metadata_subagent_non_inheriting_gemini_model() {
    let mut pool = Sticky::with(RoutingStrategy::RoundRobin, Affinity::new(MINUTE, false));
    for id in AB {
        pool.put(cred(id));
    }
    let request = |task: &str| {
        format!(
            r#"{{"model":"gemini-3.7-flash-high","metadata":{{"user_id":"{{\"device_id\":\"dev-1\",\"session_id\":\"sess-main-1\"}}"}},"messages":[{{"role":"user","content":"{task}"}}]}}"#
        )
    };
    let parent = body(&request("parent task"));
    assert_eq!(pool.pick("mixed", FLASH, &parent, &AB), "auth-a");
    assert_eq!(parent.primary(), "claude:sess-main-1");

    let agent = [("X-Claude-Code-Agent-Id", "subagent-001")];
    let subagent = session(&agent, &request("subagent 1 task"));
    assert_eq!(
        pool.pick("mixed", FLASH, &subagent, &AB),
        "auth-b",
        "the subagent took its parent's credential with subagent affinity off"
    );
    assert_eq!(subagent.primary(), "claude:sess-main-1:agent:subagent-001");

    let turn2 = session(&agent, &request("subagent 1 turn 2"));
    assert_eq!(pool.pick("mixed", FLASH, &turn2, &AB), "auth-b", "turn 2");
}

// TestSessionAffinityCodexForkWithBothSessionAndThreadIDs.
#[test]
fn codex_fork_with_both_session_and_thread_ids() {
    let ids = ["auth-codex-1", "auth-codex-2"];
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &ids);
    let parent = session(
        &[
            ("Session-Id", "parent-thread-100"),
            ("Thread-Id", "parent-thread-100"),
        ],
        "",
    );
    let parent_auth = pool.pick("openai", "model", &parent, &ids);

    let fork = session(
        &[
            ("Session-Id", "parent-thread-100"),
            ("Thread-Id", "child-thread-200"),
            (
                "X-Codex-Turn-Metadata",
                r#"{"session_id":"parent-thread-100","thread_id":"child-thread-200","forked_from_thread_id":"parent-thread-100"}"#,
            ),
        ],
        "",
    );
    assert_eq!(pool.pick("openai", "model", &fork, &ids), parent_auth);
    assert_eq!(
        fork.primary(),
        "codex:child-thread-200",
        "the fork collapsed onto its parent"
    );
    assert_eq!(fork.fallback(), "codex:parent-thread-100");
}

// TestSessionAffinityNestedMetadataForkedFromThreadID.
#[test]
fn nested_metadata_forked_from_thread_id() {
    let ids = ["auth-1", "auth-2"];
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &ids);
    let parent = body(r#"{"thread_id":"parent-t-1"}"#);
    let parent_auth = pool.pick("openai", "model", &parent, &ids);
    let fork =
        body(r#"{"thread_id":"child-t-2","metadata":{"forked_from_thread_id":"parent-t-1"}}"#);
    assert_eq!(pool.pick("openai", "model", &fork, &ids), parent_auth);
    assert!(fork.is_fork());
    assert_eq!(fork.fallback(), "thread:parent-t-1");
}

// TestSessionAffinityCodexForkWithSessionIdHeaderAndBodyThreadId.
#[test]
fn codex_fork_with_session_id_header_and_body_thread_id() {
    let ids = ["auth-1", "auth-2"];
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &ids);
    let parent = session(&[("Session-Id", "parent-sess-uuid")], "");
    let parent_auth = pool.pick("mixed", GEMINI_FLASH, &parent, &ids);

    let fork = session(
        &[("Session-Id", "parent-sess-uuid")],
        r#"{
			"thread_id": "child-thread-uuid",
			"metadata": {
				"forked_from_thread_id": "parent-sess-uuid"
			}
		}"#,
    );
    assert_eq!(pool.pick("mixed", GEMINI_FLASH, &fork, &ids), parent_auth);
    assert_eq!(
        fork.primary(),
        "codex:child-thread-uuid",
        "the fork collapsed onto its parent"
    );
    assert_eq!(fork.fallback(), "codex:parent-sess-uuid");
    assert!(fork.is_fork());
}

/// Manager settings with session affinity on, binding for `ttl`.
fn sticky_settings(ttl: Duration) -> Settings {
    Settings {
        session_affinity: true,
        session_affinity_ttl: ttl,
        ..Settings::default()
    }
}

/// The credential the manager's binding `key` holds.
pub(super) fn bound(h: &Harness, key: &str) -> Option<String> {
    let now = h.now();
    h.manager
        .lock()
        .affinity
        .as_mut()
        .and_then(|affinity| affinity.bound(key, now))
}

/// The manager's affinity pick for `model` over `providers` for `session`
/// (upstream's `pickNextMixed` with the session affinity selector).
fn pick_mixed(
    h: &Harness,
    providers: &[String],
    model: &str,
    session: Option<&Session>,
) -> Result<String, ExecError> {
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
        eligibility: Default::default(),
        tried: &tried,
    };
    let affinity = state.affinity.as_mut().expect("session affinity is on");
    selection
        .pick_next_mixed_sticky(&mut state.selector, affinity, providers, &args, session)
        .map(|picked| picked.auth.id.clone())
}

/// Ends `id`'s cooldown for `model` (upstream's
/// `expireSessionAffinityPriorityModelCooldown`).
fn expire_model_cooldown(h: &Harness, id: &str, model: &str) {
    let expired = Some(h.now() - TimeDelta::seconds(1));
    let mut state = h.manager.lock();
    let entry = state.auths.get_mut(id).expect("stored auth");
    let stored = Arc::make_mut(&mut entry.auth);
    let model_state = stored.model_states.get_mut(model).expect("model state");
    model_state.next_retry_after = expired;
    model_state.quota.next_recover_at = expired;
}

/// A 429 on `id` for `model`.
fn rate_limited(
    id: &str,
    provider: &str,
    model: &str,
    retry_after: Option<Duration>,
) -> CallResult {
    CallResult {
        auth_id: id.to_owned(),
        provider: provider.to_owned(),
        model: model.to_owned(),
        error: Some(AuthError {
            http_status: 429,
            message: "quota".into(),
            ..AuthError::default()
        }),
        retry_after,
        ..CallResult::default()
    }
}

// TestManagerSessionAffinityMixedPoolNilMetadataPropagatesFailureCleanup.
#[tokio::test(start_paused = true)]
async fn manager_mixed_pool_failure_unbinds_the_mixed_scope() {
    const MODEL: &str = "test-model";
    let h = Harness::new(sticky_settings(HOUR));
    let failing = FakeExecutor::with("affinity-p1", |_: &Call| {
        Reply::status(500, "upstream failure")
    });
    let succeeding = FakeExecutor::with("affinity-p2", |_: &Call| Reply::ok(r#"{"ok":true}"#));
    h.executor(&failing);
    h.executor(&succeeding);
    // Cooling is off, so only the affinity moves the session.
    for (id, provider) in [("auth-1", "affinity-p1"), ("auth-2", "affinity-p2")] {
        let mut a = auth_with_metadata(id, provider, serde_json::json!({"disable_cooling": true}));
        a.status = Status::Active;
        h.add(a, &[MODEL]);
    }
    let mixed = providers(&["affinity-p1", "affinity-p2"]);
    let opts = || {
        let mut opts = options();
        opts.headers = headers(&[("X-Session-Id", "sess-mixed-1")]);
        opts
    };

    // auth-1 fails, which unbinds the session, and auth-2 takes it.
    let resp = h
        .manager
        .execute(&mixed, request(MODEL), opts())
        .await
        .expect("first execute");
    assert_eq!(&resp.payload[..], br#"{"ok":true}"#);
    assert_eq!(failing.calls().len(), 1);
    assert_eq!(succeeding.calls().len(), 1);
    assert_eq!(
        bound(&h, "mixed::header:sess-mixed-1::test-model").as_deref(),
        Some("auth-2")
    );
    assert_eq!(
        bound(&h, "affinity-p1::header:sess-mixed-1::test-model"),
        None,
        "bound under the provider's scope"
    );

    // The session goes straight to auth-2.
    let resp = h
        .manager
        .execute(&mixed, request(MODEL), opts())
        .await
        .expect("second execute");
    assert_eq!(&resp.payload[..], br#"{"ok":true}"#);
    assert_eq!(failing.calls().len(), 1, "the failing credential was tried");
    assert_eq!(succeeding.calls().len(), 2);
}

// TestManagerSessionAffinityPreservesBindingAcrossHigherPriorityRecovery.
#[tokio::test(start_paused = true)]
async fn manager_preserves_binding_across_higher_priority_recovery() {
    // The mixed provider case.
    priority_recovery("affinity-priority-mixed", |h, provider, model, session| {
        pick_mixed(h, &providers(&[provider]), model, Some(session))
    });
    // The single provider case.
    priority_recovery("codex", |h, provider, model, session| {
        h.manager
            .select_auth_with_credential_policy(
                provider,
                model,
                CredentialPolicy::CodexAlphaSearchV1,
                Some(session),
            )
            .map(|picked| picked.auth.id.clone())
    });
}

/// The steps of `TestManagerSessionAffinityPreservesBindingAcrossHigherPriorityRecovery`
/// with `pick` as the pick, among Codex sign-ins for `codex`.
fn priority_recovery(
    provider: &str,
    pick: impl Fn(&Harness, &str, &str, &Session) -> Result<String, ExecError>,
) {
    const MODEL: &str = "affinity-priority-model";
    let h = Harness::new(sticky_settings(HOUR));
    h.executor(&FakeExecutor::new(provider));
    let high = format!("{provider}-high");
    let low = format!("{provider}-low");
    for (id, priority) in [(&high, "1"), (&low, "0")] {
        let mut a = auth(id, provider);
        if provider == "codex" {
            a.metadata
                .insert("access_token".into(), serde_json::json!("token"));
        }
        a.status = Status::Active;
        a.attributes.insert("priority".into(), priority.into());
        h.add(a, &[MODEL]);
    }
    let stable = derived("stable-session");
    let pick = |session: &Session| pick(&h, provider, MODEL, session).expect("pick");

    assert_eq!(pick(&stable), high, "cold binding");
    h.manager
        .mark_result(&rate_limited(&high, provider, MODEL, None));
    assert_eq!(pick(&stable), low, "failover binding");

    expire_model_cooldown(&h, &high, MODEL);
    assert_eq!(
        pick(&stable),
        low,
        "the binding moved back when the higher priority recovered"
    );
    assert_eq!(
        pick(&derived("new-session")),
        high,
        "cold binding of a new session"
    );

    h.manager
        .mark_result(&rate_limited(&low, provider, MODEL, None));
    assert_eq!(
        pick(&stable),
        high,
        "binding after the bound credential became unavailable"
    );
}

// TestManagerSessionAffinityAliasCooldownPreservesSelection, without its
// "lcp" mode.
#[tokio::test(start_paused = true)]
async fn manager_alias_cooldown_preserves_selection() {
    const ROUTE_MODEL: &str = "affinity-alias";
    const TARGET_MODEL: &str = "affinity-healthy-target";
    let strategies = [
        ("round-robin", RoutingStrategy::RoundRobin),
        ("weighted-round-robin", RoutingStrategy::Weighted),
        ("fill-first", RoutingStrategy::FillFirst),
    ];
    for (strategy_name, strategy) in strategies {
        for mode in ["no-session", "explicit-session"] {
            for path in ["select", "execute", "stream"] {
                let name = format!("{strategy_name}/{mode}/{path}");
                let h = Harness::new(Settings {
                    routing_strategy: strategy,
                    ..sticky_settings(HOUR)
                });
                // The lower tier sorts first, to catch fallbacks that pick
                // among every priority.
                let high = format!("z-high-{name}");
                let low = format!("a-low-{name}");
                let retry_after = Some(HOUR);
                let cool = |id: &str, model: &str| {
                    h.manager
                        .mark_result(&rate_limited(id, "codex", model, retry_after));
                };
                for (id, priority) in [(&high, "4"), (&low, "3")] {
                    let mut a = auth(id, "codex");
                    a.status = Status::Active;
                    a.attributes.insert("priority".into(), priority.into());
                    a.attributes.insert("weight".into(), "1".into());
                    h.add(a, &[ROUTE_MODEL, TARGET_MODEL]);
                    cool(id, ROUTE_MODEL);
                }
                // The alias comes in while both credentials cool under its
                // old name; the target is healthy on both.
                h.manager.set_settings(Settings {
                    oauth_model_alias: BTreeMap::from([(
                        "codex".to_owned(),
                        vec![ModelAlias {
                            name: TARGET_MODEL.into(),
                            alias: ROUTE_MODEL.into(),
                            force_mapping: false,
                        }],
                    )]),
                    ..(*h.manager.settings()).clone()
                });
                let executor =
                    FakeExecutor::with("codex", |call: &Call| Reply::ok(call.auth_id.clone()));
                h.executor(&executor);
                let codex = providers(&["codex"]);

                let run = |session_id: &'static str| {
                    let mut opts = options();
                    if mode == "explicit-session" {
                        opts.headers = headers(&[("X-Session-Id", session_id)]);
                    }
                    let h = &h;
                    let codex = &codex;
                    async move {
                        match path {
                            "select" => {
                                let session = Session::of(
                                    &opts.headers,
                                    &Payload::parse(&opts.original_request),
                                    "",
                                    opts.source_format.as_str(),
                                    "",
                                );
                                pick_mixed(h, codex, ROUTE_MODEL, session.as_ref())
                            }
                            "execute" => h
                                .manager
                                .execute(codex, request(ROUTE_MODEL), opts)
                                .await
                                .map(|resp| String::from_utf8_lossy(&resp.payload).into_owned()),
                            _ => {
                                opts.stream = true;
                                let stream = h
                                    .manager
                                    .execute_stream(codex, request(ROUTE_MODEL), opts)
                                    .await?;
                                let (chunks, err) = collect(stream).await;
                                assert!(err.is_none(), "stream ended with {err:?}");
                                Ok(chunks.concat())
                            }
                        }
                    }
                };
                let assert_pick = |label: &'static str, session_id: &'static str, want: String| {
                    let run = &run;
                    let executor = &executor;
                    let name = &name;
                    async move {
                        let got = run(session_id).await;
                        let attempts: Vec<String> =
                            executor.calls().into_iter().map(|c| c.auth_id).collect();
                        assert_eq!(
                            got.as_deref().ok(),
                            Some(want.as_str()),
                            "{name}: {label}: got {got:?}; upstream attempts {attempts:?}"
                        );
                    }
                };

                assert_pick(
                    "cold selection ignores the old alias cooldown",
                    "stable",
                    high.clone(),
                )
                .await;
                cool(&high, TARGET_MODEL);
                assert_pick("a target cooldown fails over", "stable", low.clone()).await;
                expire_model_cooldown(&h, &high, TARGET_MODEL);
                let after_recovery = if mode == "no-session" {
                    high.clone()
                } else {
                    low.clone()
                };
                assert_pick(
                    "higher-priority recovery keeps the binding",
                    "stable",
                    after_recovery,
                )
                .await;
                assert_pick(
                    "a fresh session takes the highest priority",
                    "fresh",
                    high.clone(),
                )
                .await;

                cool(&high, TARGET_MODEL);
                cool(&low, TARGET_MODEL);
                let before = executor.calls().len();
                let err = run("stable")
                    .await
                    .expect_err(&format!("{name}: all targets cooling"));
                assert_eq!(err.http_status(), 429, "{name}: {err}");
                assert_eq!(
                    executor.calls().len(),
                    before,
                    "{name}: called upstream while every target was cooling"
                );
                for call in executor.calls() {
                    assert_eq!(call.model, TARGET_MODEL, "{name}: upstream model");
                }
            }
        }
    }
}

// TestSessionAffinitySelectorUsesRequestPayloadWhenOriginalRequestMissing.
#[tokio::test(start_paused = true)]
async fn manager_uses_request_payload_when_original_request_missing() {
    let h = Harness::new(sticky_settings(MINUTE));
    let executor = FakeExecutor::new("openai");
    h.executor(&executor);
    for id in AB {
        h.add(auth(id, "openai"), &["gpt-test"]);
    }
    let req = request_with(
        "gpt-test",
        r#"{"conversation":{"id":"request-only-conversation"},"input":"hello"}"#,
    );
    let pool = providers(&["openai"]);
    for _ in 0..2 {
        h.manager
            .execute(&pool, req.clone(), options())
            .await
            .expect("execute");
    }
    let ids = executor.ids(Kind::Execute);
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[1], ids[0], "the request-only conversation moved");
}

// TestSessionAffinitySelector_Concurrent, with 32 Tokio tasks of 50
// manager calls each.
#[tokio::test(start_paused = true)]
async fn manager_concurrent_calls_keep_the_session() {
    const TASKS: usize = 32;
    const ITERATIONS: usize = 50;
    let h = Harness::new(sticky_settings(MINUTE));
    let executor = FakeExecutor::new("claude");
    h.executor(&executor);
    for id in ABC {
        h.add(auth(id, "claude"), &["claude-3"]);
    }
    let req = request_with(
        "claude-3",
        r#"{"metadata":{"user_id":"user_xxx_account__session_concurrent-test"}}"#,
    );
    let pool = providers(&["claude"]);
    h.manager
        .execute(&pool, req.clone(), options())
        .await
        .expect("first execute");
    let expected = executor.ids(Kind::Execute)[0].clone();

    let mut tasks = Vec::with_capacity(TASKS);
    for _ in 0..TASKS {
        let manager = h.manager.clone();
        let pool = pool.clone();
        let req = req.clone();
        tasks.push(tokio::spawn(async move {
            for j in 0..ITERATIONS {
                manager
                    .execute(&pool, req.clone(), options())
                    .await
                    .unwrap_or_else(|err| panic!("concurrent pick #{j}: {err}"));
            }
        }));
    }
    for task in tasks {
        task.await.expect("task");
    }
    let ids = executor.ids(Kind::Execute);
    assert_eq!(ids.len(), 1 + TASKS * ITERATIONS);
    assert!(
        ids.iter().all(|id| *id == expected),
        "a concurrent call moved the session"
    );
}

// Not upstream's: removing a credential drops its bindings, and the
// session picks again.
#[tokio::test(start_paused = true)]
async fn manager_remove_drops_the_bindings() {
    let h = Harness::new(sticky_settings(HOUR));
    let executor = FakeExecutor::new("openai");
    h.executor(&executor);
    for id in AB {
        h.add(auth(id, "openai"), &["test-model"]);
    }
    let mut opts = options();
    opts.headers = headers(&[("X-Session-Id", "sess-remove")]);
    let pool = providers(&["openai"]);
    let key = "mixed::header:sess-remove::test-model";
    h.manager
        .execute(&pool, request("test-model"), opts.clone())
        .await
        .expect("execute");
    let first = executor.ids(Kind::Execute)[0].clone();
    assert_eq!(bound(&h, key).as_deref(), Some(first.as_str()));

    h.manager.remove(&first);
    assert_eq!(bound(&h, key), None, "the removed credential stayed bound");
    h.manager
        .execute(&pool, request("test-model"), opts)
        .await
        .expect("execute after remove");
    let second = executor.ids(Kind::Execute)[1].clone();
    assert_ne!(second, first);
    assert_eq!(bound(&h, key).as_deref(), Some(second.as_str()));
}

// Not upstream's: a change of the routing settings forgets the bindings,
// as upstream builds a new selector; other changes keep them; and with
// session affinity off there are none.
#[tokio::test(start_paused = true)]
async fn manager_routing_change_forgets_the_bindings() {
    let h = Harness::new(sticky_settings(HOUR));
    h.executor(&FakeExecutor::new("openai"));
    h.add(auth("auth-a", "openai"), &["test-model"]);
    let mut opts = options();
    opts.headers = headers(&[("X-Session-Id", "sess-settings")]);
    let pool = providers(&["openai"]);
    let key = "mixed::header:sess-settings::test-model";
    let bind = || async {
        h.manager
            .execute(&pool, request("test-model"), opts.clone())
            .await
            .expect("execute");
    };
    bind().await;
    assert_eq!(bound(&h, key).as_deref(), Some("auth-a"));

    h.manager.set_settings(Settings {
        request_retry: 2,
        ..sticky_settings(HOUR)
    });
    assert_eq!(bound(&h, key).as_deref(), Some("auth-a"), "a retry change");

    h.manager.set_settings(sticky_settings(MINUTE));
    assert_eq!(bound(&h, key), None, "a TTL change");
    bind().await;
    h.manager.set_settings(Settings {
        session_affinity_subagents: Some(false),
        ..sticky_settings(MINUTE)
    });
    assert_eq!(bound(&h, key), None, "a subagent setting change");

    h.manager.set_settings(Settings::default());
    assert!(h.manager.lock().affinity.is_none(), "affinity off");
}
