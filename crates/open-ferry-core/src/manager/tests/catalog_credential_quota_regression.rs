// Ported from CLIProxyAPI sdk/cliproxy/auth/catalog_credential_quota_regression_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A credential-wide quota on one credential suspends all its models, while
//! another credential serving the same models stays usable, so the catalog
//! keeps listing them.
//!
//! Deviations from upstream:
//! - `credential_quota_keeps_healthy_catalog_across_restart`: the restart
//!   half (saving cooldowns to the cooldown state store and restoring them
//!   into a new manager) is dropped; the cooldown state store isn't ported.
//!   Upstream's catalog listing (`GetAvailableModels`) becomes the
//!   published projections: a model stays listed while a credential serving
//!   it has no suspended projection.

use std::time::Duration;

use super::support::*;
use crate::auth::{AuthError, Status};
use crate::manager::select::is_auth_blocked_for_model;
use crate::manager::{CallResult, Settings};

const MODELS: [&str; 3] = ["audit-luna", "audit-sol", "audit-terra"];
const CLIENTS: [&str; 2] = ["audit-healthy-api", "audit-quota-oauth"];

/// Whether some credential serving `model` hasn't had it suspended
/// (upstream's catalog listing the model).
fn listed(h: &Harness, model: &str) -> bool {
    CLIENTS.iter().any(|client| {
        h.models
            .projection(client, model)
            .is_none_or(|projection| !projection.suspended)
    })
}

#[tokio::test(start_paused = true)]
async fn credential_quota_keeps_healthy_catalog_across_restart() {
    let h = Harness::new(Settings::default());
    for id in CLIENTS {
        let mut auth = auth(id, "codex");
        auth.status = Status::Active;
        h.add(auth, &MODELS);
    }
    for model in MODELS {
        h.manager.mark_result(&CallResult {
            auth_id: "audit-quota-oauth".into(),
            provider: "codex".into(),
            model: model.into(),
            success: true,
            ..CallResult::default()
        });
    }
    h.manager.mark_result(&CallResult {
        auth_id: "audit-quota-oauth".into(),
        provider: "codex".into(),
        model: "audit-sol".into(),
        error: Some(AuthError {
            http_status: 429,
            code: "usage_limit_reached".into(),
            message: "quota exceeded".into(),
            ..AuthError::default()
        }),
        retry_after: Some(Duration::from_secs(3 * 60 * 60)),
        credential_scope: true,
        ..CallResult::default()
    });

    let oauth = h.get("audit-quota-oauth");
    for model in MODELS {
        let state = oauth
            .model_states
            .get(model)
            .filter(|state| state.quota.exceeded)
            .unwrap_or_else(|| panic!("expected model {model} to have quota.exceeded = true"));
        let expected_reason = if model == "audit-sol" {
            "quota"
        } else {
            "credential_quota"
        };
        assert_eq!(state.quota.reason, expected_reason, "model {model}");
        assert!(listed(&h, model), "healthy catalog lost {model}");
    }

    let healthy = h.get("audit-healthy-api");
    for model in MODELS {
        assert!(
            !is_auth_blocked_for_model(&healthy, model, h.now()).0,
            "healthy API provider unexpectedly blocked for {model}"
        );
    }
}
