// Ported from CLIProxyAPI sdk/cliproxy/auth/cooldown_view.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The cooldowns a credential is under, as the management API lists them:
//! the whole credential or one model, until when, and why.
//!
//! A view says only that a retry timer runs. A credential with none may
//! still be unusable (disabled, or its token expired), and one with a
//! timer may be unusable for other reasons too. Reasons are fixed codes,
//! never an upstream's message, error code or body.
//!
//! Deviations from upstream: none.

use std::collections::BTreeMap;

use super::classify::{
    is_cloudflare_challenge_result_error, is_invalid_grant_result_error,
    is_model_support_result_error,
};
use super::credential::is_zero;
use super::select::{BlockReason, availability_block};
use super::text::canonical_model_key;
use crate::auth::{Auth, AuthError, QuotaState, Timestamp};

/// One cooldown (upstream's `CooldownView`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CooldownView {
    /// `credential` or `model`.
    pub scope: &'static str,
    /// The model, for a model cooldown; empty for the credential.
    pub model_key: String,
    /// Why: `quota`, `credential_quota`, `cloudflare_challenge`,
    /// `model_not_supported`, `invalid_grant`, `unauthorized`,
    /// `payment_required`, `not_found`, `transient_error` or `unknown`.
    pub reason: &'static str,
    /// When the cooldown ends.
    pub retry_at: Timestamp,
    /// Whole seconds until then, rounded up.
    pub remaining_seconds: i64,
    /// The quota backoff level, for a quota or Cloudflare cooldown.
    pub backoff_level: Option<u32>,
    /// The HTTP status of the failure behind the cooldown, or 0.
    pub http_status: u16,
}

/// The cooldowns running on `auth` at `now`: the credential's own first,
/// then one per model in name order (upstream's `CooldownSnapshotForAuth`).
/// Model names are compared without their thinking suffix; where two states
/// name the same model, the one with the later end wins, and a quota
/// cooldown wins a tie.
pub fn cooldown_snapshot_for_auth(auth: &Auth, now: Timestamp) -> Vec<CooldownView> {
    let mut views = Vec::new();
    // Only an explicit credential-wide quota gates the whole credential;
    // other credential fields can be aggregates of the models.
    if auth.quota.exceeded
        && auth.quota.reason == "credential_quota"
        && let Some(recover_at) = auth.quota.next_recover_at.filter(|at| *at > now)
    {
        views.push(new_view(
            "credential",
            "",
            recover_at,
            now,
            &auth.quota,
            &auth.status_message,
            auth.last_error.as_ref(),
        ));
    } else if auth.model_states.is_empty()
        && let (true, _, Some(next)) = availability_block(
            auth.unavailable,
            auth.quota.exceeded,
            auth.next_retry_after,
            auth.quota.next_recover_at,
            now,
        )
        && next > now
    {
        views.push(new_view(
            "credential",
            "",
            next,
            now,
            &auth.quota,
            &auth.status_message,
            auth.last_error.as_ref(),
        ));
    }

    let mut by_model: BTreeMap<String, (CooldownView, BlockReason)> = BTreeMap::new();
    for (key, state) in &auth.model_states {
        let model = canonical_model_key(key);
        if model.is_empty() {
            continue;
        }
        let (blocked, reason, next) = availability_block(
            state.unavailable,
            state.quota.exceeded,
            state.next_retry_after,
            state.quota.next_recover_at,
            now,
        );
        let Some(next) = next.filter(|next| blocked && *next > now) else {
            continue;
        };
        if let Some((previous, previous_reason)) = by_model.get(&model) {
            let prefer_quota_tie = next == previous.retry_at
                && reason == BlockReason::Cooldown
                && *previous_reason != BlockReason::Cooldown;
            if next <= previous.retry_at && !prefer_quota_tie {
                continue;
            }
        }
        let view = new_view(
            "model",
            &model,
            next,
            now,
            &state.quota,
            &state.status_message,
            state.last_error.as_ref(),
        );
        by_model.insert(model, (view, reason));
    }
    views.extend(by_model.into_values().map(|(view, _)| view));
    views
}

/// Upstream's `newCooldownView`.
fn new_view(
    scope: &'static str,
    model: &str,
    next: Timestamp,
    now: Timestamp,
    quota: &QuotaState,
    status_message: &str,
    last_error: Option<&AuthError>,
) -> CooldownView {
    let remaining = next - now;
    let mut seconds = remaining.num_seconds();
    if remaining.subsec_nanos() != 0 {
        seconds += 1;
    }
    let mut view = CooldownView {
        scope,
        model_key: model.to_owned(),
        reason: "unknown",
        retry_at: next,
        remaining_seconds: seconds,
        backoff_level: None,
        http_status: 0,
    };
    // A shorter quota window must not label a longer retry timer of another
    // kind. Old quota state may have no recovery time at all.
    let recover_at = quota.next_recover_at.filter(|at| !is_zero(Some(*at)));
    if quota.exceeded && recover_at.is_none_or(|at| at >= next) {
        view.reason = match quota.reason.as_str() {
            "credential_quota" => "credential_quota",
            "quota" => "quota",
            "cloudflare challenge" => "cloudflare_challenge",
            _ => "unknown",
        };
    }
    let propagated_quota = quota.exceeded && quota.reason == "credential_quota";
    if view.reason == "credential_quota" {
        // The credential-wide gate wins over stale errors of its models.
        return view;
    }
    if matches!(view.reason, "quota" | "cloudflare_challenge") {
        view.backoff_level = Some(quota.backoff_level);
    }
    let error_reason = error_reason(last_error);
    if view.reason == "unknown" {
        view.reason = error_reason;
    }
    if view.reason == "unknown" {
        view.reason = status_reason(status_message);
    }
    // A propagated quota doesn't replace a sibling's error, so the error's
    // status isn't put on it.
    if let Some(err) = last_error
        && !propagated_quota
        && (400..=599).contains(&err.http_status)
        && error_reason == view.reason
    {
        view.http_status = err.http_status;
    }
    view
}

/// The reason code for a recorded failure (upstream's
/// `cooldownErrorReason`).
fn error_reason(err: Option<&AuthError>) -> &'static str {
    let Some(err) = err else {
        return "unknown";
    };
    if is_model_support_result_error(err) {
        return "model_not_supported";
    }
    if is_cloudflare_challenge_result_error(err) {
        return "cloudflare_challenge";
    }
    if is_invalid_grant_result_error(err) {
        return "invalid_grant";
    }
    match err.http_status {
        401 => "unauthorized",
        402 | 403 => "payment_required",
        404 => "not_found",
        429 => "quota",
        408 | 500 | 502 | 503 | 504 | 520..=526 => "transient_error",
        _ => status_reason(&err.code),
    }
}

/// The reason code for a status message, when it is one of the known
/// markers (upstream's `cooldownStatusReason`).
fn status_reason(message: &str) -> &'static str {
    match message.trim() {
        "quota" | "quota exhausted" => "quota",
        "cloudflare challenge" => "cloudflare_challenge",
        "invalid_grant" => "invalid_grant",
        "unauthorized" => "unauthorized",
        "payment_required" => "payment_required",
        "not_found" => "not_found",
        "model_not_supported" => "model_not_supported",
        "transient upstream error" => "transient_error",
        _ => "unknown",
    }
}
