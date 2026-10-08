// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_selection_cooldown_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The error a pick gives when no candidate is ready: a model cooldown for
//! the model the client asked for, the candidate error behind it (model
//! errors before credential errors, the latest first), and its Retry-After.
//!
//! Deviations from upstream:
//! - Upstream's errors keep the raw candidate error as their cause and
//!   summarize it only in `Error()`; here the cause is stored summarized
//!   (`ExecError::cause`, or `last_upstream_error` in a model cooldown's
//!   body), so the tests check the summary.
//!   AvailableAuthsForRouteModel_FallbackToGlobalAuthErrorWhenNoModelState
//!   looks for "credential exhausted", which the summary redacts (upstream's
//!   too), so it checks the exact summary and text upstream gives instead.
//! - BuiltInSelectorCooldownErrorPreservesRouteModel: upstream calls each
//!   built-in selector with an empty model and restores the route model on
//!   the error. The port has no separate selectors; the test takes the same
//!   path through a pick whose OAuth alias sends it down the legacy path,
//!   once per routing strategy. The credential's state for another model is
//!   left out: checked against the route model, as the real call does, it
//!   leaves the credential ready.
//! - MixedUnavailableErrorLocked_GlobalModelErrorPriorityAcrossShards:
//!   upstream calls the scheduler's error helper; here the error comes from a
//!   pick over both providers, which needs their executors.
//! - Dropped ErrorWithCause_IsDoesNotPanicOnNonComparableError: Go-only
//!   (`errors.Is` on a non-comparable error).
//! - ExtractUpstreamErrorSummary_AuthPackageDirectSanitization is ported in
//!   manager/summary.rs (`sanitizes_as_upstream_does`,
//!   `long_text_is_bounded`).

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::TimeDelta;
use serde_json::Value;

use super::support::*;
use crate::auth::{Auth, AuthError, ModelState, QuotaState, Status};
use crate::exec::{ErrorKind, ExecError};
use crate::manager::models::Resolver;
use crate::manager::select::{PickArgs, Selection, SelectorState, auth_unavailable_with_cause};
use crate::manager::{ModelAlias, RoutingStrategy, Settings};

const ROUTE_MODEL: &str = "gpt-5.6-sol";

/// Builds the selection a call would use and runs `f` with it.
fn with_selection<T>(h: &Harness, f: impl FnOnce(&Selection<'_>, &mut SelectorState) -> T) -> T {
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

/// The error a pick over `providers` for `model` gives.
fn pick_error(h: &Harness, names: &[&str], model: &str) -> ExecError {
    let providers = providers(names);
    let tried = HashSet::new();
    let args = PickArgs {
        model,
        pinned: "",
        downstream_websocket: false,
        eligibility: Default::default(),
        tried: &tried,
    };
    let picked = with_selection(h, |selection, state| {
        selection
            .pick_next_mixed(state, &providers, &args)
            .map(|picked| picked.auth.id.clone())
    });
    match picked {
        Ok(id) => panic!("pick {providers:?} {model:?} = {id}, want an error"),
        Err(err) => err,
    }
}

/// The error checking `candidates` against the route model on codex gives
/// (upstream's `availableAuthsForRouteModel`).
fn route_model_error(h: &Harness, candidates: Vec<Auth>) -> ExecError {
    let candidates: Vec<Arc<Auth>> = candidates.into_iter().map(Arc::new).collect();
    let refs: Vec<&Arc<Auth>> = candidates.iter().collect();
    let result = with_selection(h, |selection, _| {
        selection
            .available_auths_for_route_model(&refs, "codex", ROUTE_MODEL)
            .map(|ready| ready.iter().map(|auth| auth.id.clone()).collect::<Vec<_>>())
    });
    match result {
        Ok(ready) => panic!("available auths = {ready:?}, want an error"),
        Err(err) => err,
    }
}

/// The JSON body of a model cooldown error.
fn cooldown_body(err: &ExecError) -> Value {
    assert_eq!(err.kind, ErrorKind::ModelCooldown, "error = {err}");
    let body: Value = serde_json::from_str(&err.message).expect("cooldown body is JSON");
    body["error"].clone()
}

/// The summarized candidate error behind `err`.
fn cause(err: &ExecError) -> String {
    match err.cause.as_deref() {
        Some(cause) => cause.to_owned(),
        None => panic!("error {err} has no cause"),
    }
}

/// A codex credential, unavailable after an error.
fn failed(
    id: &str,
    status_message: &str,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Auth {
    let mut auth = auth(id, "codex");
    auth.unavailable = true;
    auth.status = Status::Error;
    auth.status_message = status_message.into();
    auth.updated_at = updated_at;
    auth
}

/// An unavailable state for the route model.
fn failed_state(
    status_message: &str,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
) -> ModelState {
    ModelState {
        status: Status::Error,
        unavailable: true,
        status_message: status_message.into(),
        updated_at,
        ..ModelState::default()
    }
}

#[tokio::test(start_paused = true)]
async fn built_in_selector_cooldown_error_preserves_route_model() {
    let route_model = "client-opus(high)";
    for strategy in [
        RoutingStrategy::RoundRobin,
        RoutingStrategy::Weighted,
        RoutingStrategy::FillFirst,
    ] {
        let h = Harness::new(Settings {
            routing_strategy: strategy,
            oauth_model_alias: BTreeMap::from([(
                "claude".to_owned(),
                vec![ModelAlias {
                    name: "claude-opus-4-5".into(),
                    alias: "client-opus".into(),
                    force_mapping: false,
                }],
            )]),
            ..Settings::default()
        });
        h.executor(&FakeExecutor::new("claude"));
        let next = h.now() + TimeDelta::hours(1);
        let mut cooling = auth("cooling-auth", "claude");
        cooling.unavailable = true;
        cooling.next_retry_after = Some(next);
        cooling.quota = QuotaState {
            exceeded: true,
            next_recover_at: Some(next),
            ..QuotaState::default()
        };
        let cooling = h.add(cooling, &["client-opus"]);
        // The alias makes the credential's selection model differ from the
        // route model, which sends the pick down the legacy path.
        let selection_key = with_selection(&h, |selection, _| {
            selection
                .resolver
                .selection_model_key_for_auth(&cooling, route_model)
        });
        assert_eq!(selection_key, "claude-opus-4-5", "{strategy:?}");

        let err = pick_error(&h, &["claude"], route_model);
        assert_eq!(
            cooldown_body(&err)["model"],
            route_model,
            "{strategy:?}: cooldown model"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn available_auths_for_route_model_attaches_upstream_error_when_candidates_cooling() {
    let h = Harness::new(Settings::default());
    let next = Some(h.now() + TimeDelta::minutes(1));
    let upstream_error = r#"{"type":"error","code":"server_is_overloaded","message":"Our servers are currently overloaded. Please try again later.","sequence_number":0}"#;
    let quota = QuotaState {
        exceeded: true,
        next_recover_at: next,
        ..QuotaState::default()
    };
    let last_error = AuthError {
        code: "auth_unavailable".into(),
        message: upstream_error.into(),
        http_status: 503,
        ..AuthError::default()
    };
    let mut cooling = failed("codex-auth-1", upstream_error, None);
    cooling.next_retry_after = next;
    cooling.quota = quota.clone();
    cooling.last_error = Some(last_error.clone());
    cooling.model_states.insert(
        ROUTE_MODEL.into(),
        ModelState {
            next_retry_after: next,
            quota,
            last_error: Some(last_error),
            ..failed_state(upstream_error, None)
        },
    );

    let err = route_model_error(&h, vec![cooling]);
    let body = cooldown_body(&err);
    let cause = body["last_upstream_error"]
        .as_str()
        .expect("cooldown error carries the upstream error");
    assert!(
        cause.contains("server_is_overloaded"),
        "expected cause to contain server_is_overloaded, got {cause:?}"
    );
    let text = err.to_string();
    assert!(
        text.contains("server_is_overloaded"),
        "expected cooldown error string to contain server_is_overloaded, got {text:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn available_auths_for_route_model_picks_latest_candidate_error() {
    let h = Harness::new(Settings::default());
    let now = h.now();
    let older = "older error: quota exceeded";
    let newer = "newer error: server is overloaded";
    let older_at = Some(now - TimeDelta::minutes(10));
    let newer_at = Some(now - TimeDelta::seconds(10));
    let mut auth1 = failed("codex-auth-1", older, older_at);
    auth1
        .model_states
        .insert(ROUTE_MODEL.into(), failed_state(older, older_at));
    let mut auth2 = failed("codex-auth-2", newer, newer_at);
    auth2
        .model_states
        .insert(ROUTE_MODEL.into(), failed_state(newer, newer_at));

    let err = route_model_error(&h, vec![auth1, auth2]);
    let cause = cause(&err);
    assert!(
        cause.contains("newer error"),
        "expected latest error (newer error), got {cause:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn available_auths_for_route_model_model_error_prioritized_over_global_auth_error() {
    let h = Harness::new(Settings::default());
    let now = h.now();
    let mut auth = failed("codex-auth-1", "global error: unrelated failure", Some(now));
    auth.model_states.insert(
        ROUTE_MODEL.into(),
        failed_state(
            "model error: model rate limited",
            Some(now - TimeDelta::minutes(10)),
        ),
    );

    let err = route_model_error(&h, vec![auth]);
    let cause = cause(&err);
    assert!(
        cause.contains("model rate limited"),
        "expected model error to be prioritized over global auth error, got {cause:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn available_auths_for_route_model_fallback_to_global_auth_error_when_no_model_state() {
    let h = Harness::new(Settings::default());
    let auth = failed(
        "codex-auth-1",
        "global error: credential exhausted",
        Some(h.now()),
    );

    // Upstream checks the raw cause for "credential exhausted"; its summary,
    // the only form kept here, redacts the word after "credential", so the
    // test checks the summary and text upstream gives for that cause.
    let err = route_model_error(&h, vec![auth]);
    let cause = cause(&err);
    assert_eq!(
        cause, "global error: credential: [REDACTED]",
        "expected fallback to global auth error"
    );
    assert_eq!(
        err.to_string(),
        "auth_unavailable: no auth available (last upstream error: global error: credential: [REDACTED])"
    );
}

#[tokio::test(start_paused = true)]
async fn available_auths_for_route_model_cross_candidate_model_error_prioritized_over_newer_auth_error()
 {
    let h = Harness::new(Settings::default());
    let now = h.now();
    let older_at = Some(now - TimeDelta::minutes(10));
    let mut auth_a = failed("codex-auth-A", "older global status A", older_at);
    auth_a.model_states.insert(
        ROUTE_MODEL.into(),
        failed_state("model error on candidate A: quota exceeded", older_at),
    );
    // Newer, but only a credential-level error.
    let auth_b = failed(
        "codex-auth-B",
        "global auth error on candidate B: connection reset",
        Some(now - TimeDelta::seconds(10)),
    );

    let err = route_model_error(&h, vec![auth_a, auth_b]);
    let cause = cause(&err);
    assert!(
        cause.contains("quota exceeded"),
        "expected candidate A model-scoped error to be strictly prioritized over candidate B auth-level error, got {cause:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn safe_response_headers_includes_model_cooldown_retry_after() {
    let err =
        ExecError::model_cooldown("gpt-5.6-sol", Some("codex"), Duration::from_secs(15), None);
    let header = err
        .retry_after_header()
        .expect("model cooldown sets Retry-After");
    assert_eq!(header, "15");
}

#[tokio::test(start_paused = true)]
async fn mixed_unavailable_error_locked_global_model_error_priority_across_shards() {
    let h = Harness::new(Settings::default());
    h.executor(&FakeExecutor::new("codex"));
    h.executor(&FakeExecutor::new("gemini"));
    let now = h.now();
    let older_at = Some(now - TimeDelta::minutes(10));

    let mut codex = failed("auth-codex", "older codex global status", older_at);
    codex.model_states.insert(
        ROUTE_MODEL.into(),
        failed_state("codex model error: rate limited", older_at),
    );
    h.add(codex, &[ROUTE_MODEL]);
    // Newer, but only a credential-level error on gemini.
    let mut gemini = failed(
        "auth-gemini",
        "gemini global error: network timeout",
        Some(now - TimeDelta::seconds(10)),
    );
    gemini.provider = "gemini".into();
    h.add(gemini, &[ROUTE_MODEL]);

    let err = pick_error(&h, &["codex", "gemini"], ROUTE_MODEL);
    let cause = cause(&err);
    assert!(
        cause.contains("rate limited"),
        "expected codex model-level error to be prioritized across shards over gemini auth-level error, got {cause:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn error_with_cause_error_direct_format() {
    let h = Harness::new(Settings::default());
    let raw = r#"{"type":"error","code":"server_is_overloaded","message":"Our servers are currently overloaded. Please try again later.","sequence_number":0}"#;
    let got = auth_unavailable_with_cause(None, h.now(), Some(raw)).to_string();
    assert!(
        got.contains("auth_unavailable: no auth available"),
        "Error() = {got:?}, want containing base error"
    );
    assert!(
        got.contains("server_is_overloaded")
            && got.contains("Our servers are currently overloaded. Please try again later."),
        "Error() = {got:?}, want containing upstream error summary"
    );
}
