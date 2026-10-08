// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_cooldown_monotonic_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A later failure never shortens a live cooldown, only extends it; a
//! credential-scoped 429 neither inherits nor promotes per-model deadlines or
//! backoff levels; the client projection agrees with scheduling.
//!
//! Deviations from upstream:
//! - `TestManager_MarkResult_CredentialScopeBackoffPersistsAcrossWindows`
//!   expires the stored deadlines through the manager's state lock, as
//!   upstream does under `m.mu`.

use super::support::*;

use std::sync::Arc;
use std::time::Duration;

use chrono::TimeDelta;

use crate::auth::{Auth, AuthError, ModelState};
use crate::manager::cooldown::{
    QUOTA_BACKOFF_BASE, client_model_projection_for_auth, existing_model_state,
};
use crate::manager::models::Resolver;
use crate::manager::select::is_auth_blocked_for_model;
use crate::manager::{CallResult, Settings};

/// A manager with one `claude` credential serving `models` (upstream's
/// `newCooldownMonotonicManager`).
fn new_cooldown_monotonic_manager(models: &[&str]) -> (Harness, Arc<Auth>) {
    let h = Harness::new(Settings::default());
    let a = h.add(
        auth(&format!("auth-monotonic-{}", models[0]), "claude"),
        models,
    );
    (h, a)
}

fn error(status: u16, message: &str) -> AuthError {
    AuthError {
        http_status: status,
        message: message.into(),
        ..AuthError::default()
    }
}

fn fail(auth: &Auth, model: &str, err: AuthError) -> CallResult {
    CallResult {
        auth_id: auth.id.clone(),
        provider: auth.provider.clone(),
        model: model.into(),
        success: false,
        error: Some(err),
        ..CallResult::default()
    }
}

fn fail_with(
    auth: &Auth,
    model: &str,
    retry_after: Option<Duration>,
    credential_scope: bool,
    err: AuthError,
) -> CallResult {
    CallResult {
        retry_after,
        credential_scope,
        ..fail(auth, model, err)
    }
}

fn state(auth: &Auth, model: &str) -> Option<ModelState> {
    existing_model_state(auth, model).cloned()
}

fn mins(n: i64) -> TimeDelta {
    TimeDelta::minutes(n)
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_credential_scope_keeps_longer_sibling_deadline() {
    let (h, a) = new_cooldown_monotonic_manager(&["model-a", "model-b"]);

    // A long per-model deadline on model-b: a 401 (~30m).
    h.manager
        .mark_result(&fail(&a, "model-b", error(401, "long 401")));
    let before = h.now();
    let sibling = h.get(&a.id);
    let b_state = state(&sibling, "model-b");
    assert!(
        b_state
            .as_ref()
            .and_then(|s| s.next_retry_after)
            .is_some_and(|t| t >= before + mins(25)),
        "precondition failed: model-b long deadline missing: {b_state:?}"
    );

    // A shorter credential-scoped 429 on model-a must not shorten model-b.
    h.manager.mark_result(&fail_with(
        &a,
        "model-a",
        Some(Duration::from_secs(5 * 60)),
        true,
        error(429, "credential 429"),
    ));

    let updated = h.get(&a.id);
    let b_after = state(&updated, "model-b").expect("model-b state missing after sibling failure");
    assert!(
        b_after
            .next_retry_after
            .is_some_and(|t| t >= before + mins(25)),
        "credential-scoped failure shortened model-b deadline to {:?}",
        b_after.next_retry_after.map(|t| t - before)
    );

    // model-a is blocked only for the 5m credential quota, not model-b's 30m.
    let (blocked_a, _, _) = is_auth_blocked_for_model(&updated, "model-a", before + mins(6));
    assert!(
        !blocked_a,
        "model-a should have unblocked after its 5m credential quota, but is still blocked"
    );
    let (blocked_b, _, _) = is_auth_blocked_for_model(&updated, "model-b", before + mins(6));
    assert!(
        blocked_b,
        "model-b should still be blocked after 6 minutes due to its 30m deadline"
    );
}

// A sibling's long non-quota deadline (a 12h 404) must not be promoted into
// its quota recovery time, or later credential-scoped writes on that sibling
// would push every model to 12h.
#[tokio::test(start_paused = true)]
async fn manager_mark_result_credential_scope_does_not_promote_sibling_deadline_to_quota() {
    let (h, a) = new_cooldown_monotonic_manager(&["model-a", "model-b"]);
    let short = Some(Duration::from_secs(5 * 60));

    h.manager
        .mark_result(&fail(&a, "model-b", error(404, "model not found")));
    let before = h.now();

    h.manager.mark_result(&fail_with(
        &a,
        "model-a",
        short,
        true,
        error(429, "credential 429"),
    ));

    let snap1 = h.get(&a.id);
    let b1 = state(&snap1, "model-b").expect("model-b state missing");
    assert!(
        b1.next_retry_after
            .is_some_and(|t| t >= before + TimeDelta::hours(11)),
        "model-b next_retry_after shortened: {:?}",
        b1.next_retry_after.map(|t| t - before)
    );
    // Go's zero time is never after anything, so an unset recovery time passes.
    assert!(
        !b1.quota
            .next_recover_at
            .is_some_and(|t| t > before + mins(10)),
        "model-b quota.next_recover_at was incorrectly elevated to {:?}",
        b1.quota.next_recover_at.map(|t| t - before)
    );

    // A later in-flight request on model-b also gets a credential-scoped 429.
    h.manager.mark_result(&fail_with(
        &a,
        "model-b",
        short,
        true,
        error(429, "credential 429"),
    ));

    let snap2 = h.get(&a.id);
    let a2 = state(&snap2, "model-a").expect("model-a state missing");
    assert!(
        !a2.next_retry_after.is_some_and(|t| t > before + mins(10)),
        "model-a was incorrectly elevated to 12h: next_retry_after={:?}",
        a2.next_retry_after.map(|t| t - before)
    );
    assert!(
        !snap2
            .quota
            .next_recover_at
            .is_some_and(|t| t > before + mins(10)),
        "auth quota.next_recover_at was incorrectly elevated to 12h: {:?}",
        snap2.quota.next_recover_at.map(|t| t - before)
    );

    let (blocked_a, _, _) = is_auth_blocked_for_model(&snap2, "model-a", before + mins(6));
    assert!(
        !blocked_a,
        "model-a should be unblocked after 6m, but is blocked"
    );
    let (blocked_b, _, _) = is_auth_blocked_for_model(&snap2, "model-b", before + mins(6));
    assert!(blocked_b, "model-b should still be blocked after 6m");
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_later_shorter_failure_keeps_longer_model_deadline() {
    let cases: [(&str, Option<Duration>, AuthError); 2] = [
        (
            "401_then_short_429",
            Some(Duration::from_secs(2 * 60)),
            error(429, "short 429"),
        ),
        ("401_then_transient_500", None, error(500, "transient 500")),
    ];
    for (name, retry_after, second) in cases {
        let (h, a) = new_cooldown_monotonic_manager(&["model-a"]);
        h.manager
            .mark_result(&fail(&a, "model-a", error(401, "long 401")));
        let before = h.now();
        let snap = h.get(&a.id);
        let st = state(&snap, "model-a");
        assert!(
            st.as_ref()
                .and_then(|s| s.next_retry_after)
                .is_some_and(|t| t >= before + mins(25)),
            "{name}: precondition failed: 401 deadline missing: {st:?}"
        );

        h.manager
            .mark_result(&fail_with(&a, "model-a", retry_after, false, second));

        let updated = h.get(&a.id);
        let after = state(&updated, "model-a").expect("model state missing after second writer");
        assert!(
            after
                .next_retry_after
                .is_some_and(|t| t >= before + mins(25)),
            "{name}: second writer shortened live deadline to {:?}",
            after.next_retry_after.map(|t| t - before)
        );
    }
}

// The client projection must agree with scheduling: a live credential-wide
// cooldown suspends models that carry no per-model state.
#[tokio::test(start_paused = true)]
async fn manager_client_model_projection_reflects_auth_level_cooldown() {
    let (h, a) = new_cooldown_monotonic_manager(&["model-a"]);

    h.manager
        .mark_result(&fail(&a, "", error(401, "credential-wide 401")));

    let snap = h.get(&a.id);
    assert!(
        snap.unavailable
            && snap
                .next_retry_after
                .is_some_and(|t| t >= h.now() + mins(25)),
        "precondition failed: expected live auth-level cooldown, got unavailable={} next_retry_after={:?}",
        snap.unavailable,
        snap.next_retry_after
    );

    let (blocked, _, _) = is_auth_blocked_for_model(&snap, "model-a", h.now());
    let (settings, oauth) = h.manager.resolver_parts();
    let resolver = Resolver {
        settings: &settings,
        oauth: &oauth,
    };
    let projection = client_model_projection_for_auth(resolver, &snap, "model-a", h.now());
    assert!(
        !blocked || projection.suspended,
        "scheduling blocks the model while projection reports suspended=false: {projection:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_apply_auth_failure_state_preserves_longer_credential_deadline() {
    let cases = [
        ("404_then_invalid_grant", error(400, "invalid_grant")),
        (
            "404_then_cloudflare",
            error(403, "just a moment... cloudflare challenge"),
        ),
        (
            "404_then_transient_500",
            error(500, "internal server error"),
        ),
    ];
    for (name, second) in cases {
        let (h, a) = new_cooldown_monotonic_manager(&["model-a"]);
        // A credential-wide 404 (12h deadline).
        h.manager
            .mark_result(&fail(&a, "", error(404, "credential not found")));
        let before = h.now();
        let snap = h.get(&a.id);
        assert!(
            snap.unavailable
                && snap
                    .next_retry_after
                    .is_some_and(|t| t >= before + TimeDelta::hours(11)),
            "{name}: precondition failed: expected ~12h deadline, got {:?}",
            snap.next_retry_after.map(|t| t - before)
        );

        // A shorter credential failure follows.
        h.manager.mark_result(&fail(&a, "", second));

        let updated = h.get(&a.id);
        assert!(
            updated
                .next_retry_after
                .is_some_and(|t| t >= before + TimeDelta::hours(11)),
            "{name}: shorter failure shortened credential-level deadline to {:?}",
            updated.next_retry_after.map(|t| t - before)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_credential_scope_does_not_inherit_model_quota_deadline() {
    let cases = [
        ("cross_model", "claude-sonnet-5"),
        ("same_model", "claude-fable-5-1"),
    ];
    for (name, credential_model) in cases {
        let (h, a) = new_cooldown_monotonic_manager(&[
            "claude-fable-5-1",
            "claude-sonnet-5",
            "claude-opus-5-5",
            "claude-sonnet-4",
        ]);
        for model in ["claude-fable-5-1", "claude-sonnet-5", "claude-opus-5-5"] {
            h.manager.mark_result(&CallResult {
                auth_id: a.id.clone(),
                provider: a.provider.clone(),
                model: model.into(),
                success: true,
                ..CallResult::default()
            });
        }

        let before = h.now();
        h.manager.mark_result(&fail_with(
            &a,
            "claude-fable-5-1",
            Some(Duration::from_secs(8 * 24 * 60 * 60)),
            false,
            error(429, "Usage credits are required for this model."),
        ));

        let model_only = h.get(&a.id);
        assert!(
            model_only.quota.reason == "quota"
                && model_only
                    .quota
                    .next_recover_at
                    .is_some_and(|t| t > before + TimeDelta::days(7)),
            "{name}: precondition failed: model quota deadline was not aggregated: quota={:?}",
            model_only.quota
        );
        let (blocked, _, _) = is_auth_blocked_for_model(&model_only, "claude-opus-5-5", h.now());
        assert!(
            !blocked,
            "{name}: model-only cooldown should not block an unrelated model"
        );

        let credential_retry = Duration::from_secs(3 * 60 * 60);
        h.manager.mark_result(&fail_with(
            &a,
            credential_model,
            Some(credential_retry),
            true,
            error(429, "shared window rejected"),
        ));

        let updated = h.get(&a.id);
        let credential_retry_delta = TimeDelta::hours(3);
        let min_deadline = before + credential_retry_delta - TimeDelta::hours(1);
        let max_deadline = before + credential_retry_delta + TimeDelta::hours(1);
        assert!(
            updated.quota.reason == "credential_quota"
                && updated
                    .quota
                    .next_recover_at
                    .is_some_and(|t| t >= min_deadline && t <= max_deadline),
            "{name}: credential cooldown inherited a model-only deadline: reason={:?} deadline={:?}",
            updated.quota.reason,
            updated.quota.next_recover_at.map(|t| t - before)
        );

        let probe = before + credential_retry_delta + mins(1);
        let (opus_blocked, _, opus_next) =
            is_auth_blocked_for_model(&updated, "claude-opus-5-5", probe);
        assert!(
            !opus_blocked,
            "{name}: opus remained blocked beyond credential cooldown: next={opus_next:?}"
        );
        let (fable_blocked, _, fable_next) =
            is_auth_blocked_for_model(&updated, "claude-fable-5-1", probe);
        assert!(
            fable_blocked && fable_next.is_some_and(|t| t > before + TimeDelta::days(7)),
            "{name}: model-only cooldown was not retained for Fable: blocked={fable_blocked} next={fable_next:?}"
        );

        let credential_deadline = updated.quota.next_recover_at;
        h.manager.mark_result(&fail_with(
            &a,
            "claude-sonnet-4",
            Some(Duration::from_secs(30 * 60)),
            true,
            error(429, "shorter shared window rejection"),
        ));
        let updated = h.get(&a.id);
        assert_eq!(
            updated.quota.next_recover_at, credential_deadline,
            "{name}: later credential-scoped cooldown shortened the active credential deadline"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_credential_scope_backoff_persists_across_windows() {
    let (h, a) = new_cooldown_monotonic_manager(&["model-a", "model-b"]);
    h.manager.mark_result(&fail_with(
        &a,
        "model-a",
        None,
        true,
        error(429, "credential quota exceeded"),
    ));

    let updated = h.get(&a.id);
    assert_eq!(
        updated.quota.backoff_level, 1,
        "first credential quota failure did not retain backoff level 1: quota={:?}",
        updated.quota
    );

    let expired = Some(h.now() - mins(1));
    {
        let mut state = h.manager.lock();
        let entry = state.auths.get_mut(&a.id).expect("stored auth");
        let stored = Arc::make_mut(&mut entry.auth);
        stored.quota.next_recover_at = expired;
        stored.next_retry_after = expired;
        for model_state in stored.model_states.values_mut() {
            model_state.quota.next_recover_at = expired;
            model_state.next_retry_after = expired;
        }
    }

    let second_failure_at = h.now();
    h.manager.mark_result(&fail_with(
        &a,
        "model-b",
        None,
        true,
        error(429, "credential quota exceeded again"),
    ));

    let updated = h.get(&a.id);
    assert_eq!(
        updated.quota.backoff_level, 2,
        "second credential quota failure did not advance backoff to level 2: quota={:?}",
        updated.quota
    );
    let floor = second_failure_at + TimeDelta::from_std(2 * QUOTA_BACKOFF_BASE).expect("delta");
    assert!(
        updated.quota.next_recover_at.is_some_and(|t| t >= floor),
        "second credential quota failure did not use the next backoff window: deadline={:?}",
        updated.quota.next_recover_at
    );
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_credential_scope_does_not_inherit_model_backoff_level() {
    let hint = Duration::from_secs(10);
    let cases = [
        ("without_retry_hint", None, 1, QUOTA_BACKOFF_BASE),
        ("with_retry_hint", Some(hint), 0, hint),
    ];
    for (name, retry_after, want_level, want_cooldown) in cases {
        let (h, a) = new_cooldown_monotonic_manager(&["model-a", "model-b"]);
        h.manager.mark_result(&fail(
            &a,
            "model-a",
            error(429, "model-only quota exceeded"),
        ));

        let model_only = h.get(&a.id);
        let model_state = state(&model_only, "model-a");
        assert!(
            model_only.quota.reason == "quota"
                && model_state
                    .as_ref()
                    .is_some_and(|s| s.quota.backoff_level == 1),
            "{name}: precondition failed: model-only quota/backoff missing: auth={:?} state={model_state:?}",
            model_only.quota
        );

        let started = h.now();
        h.manager.mark_result(&fail_with(
            &a,
            "model-a",
            retry_after,
            true,
            error(429, "credential quota exceeded"),
        ));
        let completed = h.now();

        let updated = h.get(&a.id);
        assert_eq!(
            updated.quota.backoff_level, want_level,
            "{name}: credential cooldown inherited model backoff"
        );
        let want = TimeDelta::from_std(want_cooldown).expect("delta");
        let min_deadline = started + want;
        let max_deadline = completed + want;
        assert!(
            updated
                .quota
                .next_recover_at
                .is_some_and(|t| t >= min_deadline && t <= max_deadline),
            "{name}: credential cooldown deadline={:?}, want between {min_deadline} and {max_deadline}",
            updated.quota.next_recover_at
        );
    }
}
