// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_issue6415_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! An access token the provider refused with a 401, when the credential
//! doesn't know when it expires, counts as expired: the credential stays
//! out of rotation while it refreshes, and after a refresh that failed,
//! until a refresh works or its tokens change. Clearing the 401 leaves a
//! model's running quota cooldown alone.
//!
//! Deviations from upstream:
//! - The refresh waits on a delay in paused Tokio time, where upstream
//!   holds it on a channel. Upstream's `refreshAuthForRequest(ctx, id,
//!   token)` is `refresh_at_epoch(id, token, 0)`.
//! - A canceled refresh is an [`ExecError`] of kind
//!   [`ErrorKind::Canceled`], upstream's `context.Canceled`.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::TimeDelta;
use serde_json::json;

use super::conductor_unauthorized_refresh::{
    BACKUP, MODEL, PRIMARY, new_unauthorized_refresh_fixture, plain_error,
};
use super::support::*;
use crate::auth::{AuthError, ModelState, QuotaState, Status};
use crate::exec::{Dispatcher, ErrorKind, ExecError};
use crate::manager::credential::access_token;
use crate::manager::select::is_auth_blocked_for_model;
use crate::manager::{ManagerError, lock};

#[tokio::test(start_paused = true)]
async fn refresh_pending_does_not_select_rejected_credential() {
    let (h, executor, _tokens) = new_unauthorized_refresh_fixture(false);
    executor.set_refresh_delay(Duration::from_secs(10));

    let manager = h.manager.clone();
    let first = tokio::spawn(async move {
        manager
            .execute(&providers(&["codex"]), request(MODEL), options())
            .await
    });
    // The primary's 401 started a refresh, which is now waiting.
    while executor.refresh_count() == 0 {
        tokio::task::yield_now().await;
    }

    assert!(
        is_auth_blocked_for_model(&h.get(PRIMARY), MODEL, h.now()).0,
        "primary must be blocked while refresh is pending for rejected token"
    );

    let before = executor.ids(Kind::Execute).len();
    let resp = h
        .manager
        .execute(&providers(&["codex"]), request(MODEL), options())
        .await
        .expect("concurrent execute, want success via backup");
    assert_eq!(
        &resp.payload[..],
        format!("{BACKUP}:backup-access-token").as_bytes()
    );
    assert!(
        !executor.ids(Kind::Execute)[before..]
            .iter()
            .any(|id| id == PRIMARY),
        "primary was executed during pending refresh"
    );

    let resp = first
        .await
        .expect("task")
        .expect("first execute, want success after refresh");
    assert_eq!(
        &resp.payload[..],
        format!("{PRIMARY}:fresh-access-token").as_bytes()
    );
}

#[tokio::test(start_paused = true)]
async fn non_terminal_refresh_failure_marks_credential_unavailable() {
    let (h, _executor, tokens) = new_unauthorized_refresh_fixture(false);
    lock(&tokens).refresh_err = Some(plain_error("upstream 503 Service Unavailable"));

    let token = access_token(&h.get(PRIMARY));
    h.manager
        .refresh_at_epoch(PRIMARY, &token, 0)
        .await
        .expect_err("expected refresh error");

    let primary = h.get(PRIMARY);
    assert!(
        primary.unavailable,
        "primary must be unavailable after its rejected token's refresh failed"
    );
    assert_eq!(primary.status, Status::Error);
}

/// A model state cooling down for quota until `recover`, whose last error
/// is `error`.
fn quota_state(message: &str, error: AuthError, recover: crate::auth::Timestamp) -> ModelState {
    ModelState {
        status: Status::Error,
        status_message: message.into(),
        unavailable: true,
        next_retry_after: Some(recover),
        quota: QuotaState {
            exceeded: true,
            reason: "quota".into(),
            next_recover_at: Some(recover),
            ..QuotaState::default()
        },
        last_error: Some(error),
        ..ModelState::default()
    }
}

#[tokio::test(start_paused = true)]
async fn refresh_success_preserves_active_quota_cooldown() {
    let (h, _executor, _tokens) = new_unauthorized_refresh_fixture(false);

    let recover = h.now() + TimeDelta::minutes(30);
    let mut primary = (*h.get(PRIMARY)).clone();
    primary.model_states = BTreeMap::from([(
        MODEL.to_owned(),
        quota_state(
            "unauthorized",
            AuthError {
                http_status: 401,
                message: "unauthorized".into(),
                ..AuthError::default()
            },
            recover,
        ),
    )]);
    h.manager.update(primary).expect("update primary auth");

    h.manager
        .force_refresh(PRIMARY)
        .await
        .expect("force refresh");

    let state = h.get(PRIMARY).model_states.get(MODEL).cloned();
    let state = state.expect("model state after refresh");
    assert!(
        state.quota.exceeded,
        "quota cooldown was discarded: {state:?}"
    );
    assert_eq!(state.quota.next_recover_at, Some(recover));
    assert!(state.last_error.is_none(), "the 401 stays: {state:?}");
    assert_eq!(state.status_message, "quota");
}

#[tokio::test(start_paused = true)]
async fn execute_retry_success_preserves_active_quota_cooldown() {
    let (h, executor, _tokens) = new_unauthorized_refresh_fixture(false);
    let sibling = "gpt-5-mini";
    h.models.register(PRIMARY, &[MODEL, sibling]);

    let recover = h.now() + TimeDelta::minutes(45);
    let mut primary = (*h.get(PRIMARY)).clone();
    primary.model_states = BTreeMap::from([(
        sibling.to_owned(),
        quota_state(
            "quota_exceeded",
            AuthError {
                http_status: 429,
                message: "429 rate limited".into(),
                ..AuthError::default()
            },
            recover,
        ),
    )]);
    h.manager.update(primary).expect("update primary auth");

    let resp = h
        .manager
        .execute(&providers(&["codex"]), request(MODEL), options())
        .await
        .expect("execute, want success after refresh retry");
    assert_eq!(
        &resp.payload[..],
        format!("{PRIMARY}:fresh-access-token").as_bytes()
    );
    assert_eq!(executor.refresh_count(), 1, "refresh calls");

    let state = h.get(PRIMARY).model_states.get(sibling).cloned();
    let state = state.expect("sibling model state");
    assert!(state.quota.exceeded, "sibling cooldown cleared: {state:?}");
    assert_eq!(state.quota.next_recover_at, Some(recover));
}

#[tokio::test(start_paused = true)]
async fn mark_rejected_access_token_advances_generation() {
    let (h, _executor, _tokens) = new_unauthorized_refresh_fixture(false);
    let initial = h.get(PRIMARY);
    let token = access_token(&initial);

    h.manager.mark_rejected_access_token(PRIMARY, &token);

    let after = h.get(PRIMARY);
    assert!(
        after.generation > initial.generation,
        "generation = {}, want > {}",
        after.generation,
        initial.generation
    );
    assert_eq!(after.rejected_access_token, token);
}

// Not upstream's: a token that is no longer the access token, or one with
// an expiry of its own, isn't marked.
#[tokio::test(start_paused = true)]
async fn mark_rejected_access_token_needs_the_current_token_without_expiry() {
    let (h, _executor, _tokens) = new_unauthorized_refresh_fixture(false);
    let initial = h.get(PRIMARY);

    h.manager
        .mark_rejected_access_token(PRIMARY, "another-token");
    let after = h.get(PRIMARY);
    assert_eq!(after.generation, initial.generation);
    assert!(after.rejected_access_token.is_empty());

    let mut expiring = (*after).clone();
    expiring.metadata.insert(
        "expired".into(),
        json!((h.now() + TimeDelta::hours(1)).to_rfc3339()),
    );
    h.manager.update(expiring).expect("update");
    h.manager
        .mark_rejected_access_token(PRIMARY, &access_token(&initial));
    assert!(h.get(PRIMARY).rejected_access_token.is_empty());
}

#[tokio::test(start_paused = true)]
async fn update_without_credential_change_preserves_rejected_access_token() {
    let (h, _executor, _tokens) = new_unauthorized_refresh_fixture(false);
    let token = access_token(&h.get(PRIMARY));
    h.manager.mark_rejected_access_token(PRIMARY, &token);

    // A reload from the file, with the same tokens: the file doesn't hold
    // the marker.
    let mut reload = (*h.get(PRIMARY)).clone();
    reload.rejected_access_token.clear();
    reload
        .metadata
        .insert("note".into(), json!("reloaded from file"));
    let updated = h.manager.update(reload).expect("update").expect("auth");
    assert_eq!(updated.rejected_access_token, token);
    assert_eq!(h.get(PRIMARY).rejected_access_token, token);

    // Not upstream's: new tokens clear it.
    let mut rotated = (*h.get(PRIMARY)).clone();
    rotated
        .metadata
        .insert("access_token".into(), json!("rotated-access-token"));
    let updated = h.manager.update(rotated).expect("update").expect("auth");
    assert!(updated.rejected_access_token.is_empty());
}

#[tokio::test(start_paused = true)]
async fn refresh_success_with_unchanged_token_clears_rejected_access_token() {
    let (h, _executor, tokens) = new_unauthorized_refresh_fixture(false);
    let token = access_token(&h.get(PRIMARY));
    h.manager.mark_rejected_access_token(PRIMARY, &token);

    // The refresh hands back the same access token.
    lock(&tokens)
        .refresh_tokens
        .insert(PRIMARY.to_owned(), token.clone());

    let refreshed = h
        .manager
        .force_refresh(PRIMARY)
        .await
        .expect("force refresh");
    assert!(refreshed.rejected_access_token.is_empty());
    let fetched = h.get(PRIMARY);
    assert!(fetched.rejected_access_token.is_empty());
    assert!(
        !is_auth_blocked_for_model(&fetched, MODEL, h.now()).0,
        "primary is blocked after successful refresh with unchanged token"
    );
}

#[tokio::test(start_paused = true)]
async fn refresh_canceled_reschedules_refresh() {
    let (h, _executor, tokens) = new_unauthorized_refresh_fixture(false);
    lock(&tokens).refresh_err = Some(ExecError::new(ErrorKind::Canceled, "context canceled"));

    let token = access_token(&h.get(PRIMARY));
    let err = h
        .manager
        .refresh_at_epoch(PRIMARY, &token, 0)
        .await
        .expect_err("expected canceled error");
    assert!(
        matches!(&err, ManagerError::Refresh(err) if err.kind == ErrorKind::Canceled),
        "err = {err}, want canceled"
    );
    assert!(
        h.get(PRIMARY).next_refresh_after.is_some(),
        "next refresh should be scheduled after refresh cancellation"
    );
}
