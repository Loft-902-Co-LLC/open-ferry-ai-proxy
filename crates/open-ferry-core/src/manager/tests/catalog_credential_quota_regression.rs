// Ported from CLIProxyAPI sdk/cliproxy/auth/catalog_credential_quota_regression_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A credential-wide quota on one credential suspends all its models, while
//! another credential serving the same models stays usable, so the catalog
//! keeps listing them.
//!
//! Deviations from upstream:
//! - Upstream's catalog listing (`GetAvailableModels`) becomes the published
//!   projections: a model stays listed while a credential serving it has no
//!   suspended projection.
//! - The restart installs a cooldown `FileStore` on the first manager, and
//!   on the new one, in a temporary directory, with `install_store` and
//!   `restore_now`. The first manager flushes where upstream's would have
//!   saved. The restart's registry is a new `FakeModels`, where upstream
//!   unregisters and registers the clients in its global one.

use std::sync::Arc;
use std::time::Duration;

use super::support::*;
use crate::auth::{AuthError, Status};
use crate::manager::cooldown_store::{self, FileStore};
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

/// A manager with `CLIENTS` registered for `MODELS`, saving its cooldowns
/// to `dir` when flushed.
fn harness(dir: &std::path::Path) -> Harness {
    let h = Harness::new(Settings::default());
    cooldown_store::set_debounce(&h.manager, Duration::from_secs(3600));
    for id in CLIENTS {
        let mut auth = auth(id, "codex");
        auth.status = Status::Active;
        h.add(auth, &MODELS);
    }
    cooldown_store::install_store(&h.manager, Arc::new(FileStore::new(dir.to_path_buf())));
    cooldown_store::restore_now(&h.manager);
    h
}

#[tokio::test(start_paused = true)]
async fn credential_quota_keeps_healthy_catalog_across_restart() {
    let dir = tempfile::tempdir().expect("temp dir");
    let h = harness(dir.path());
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

    // Restart over the same `.cds` files: the clients register with the
    // models, then the cooldowns come back and the registry is reconciled.
    cooldown_store::flush(&h.manager);
    let restarted = harness(dir.path());
    restarted
        .manager
        .reconcile_registry_model_states("audit-quota-oauth");
    let restored = restarted.get("audit-quota-oauth");
    assert!(
        restored.quota.reason == "credential_quota"
            && restored
                .quota
                .next_recover_at
                .is_some_and(|at| at > restarted.now()),
        "restart did not restore the active credential quota: {:?}",
        restored.quota
    );
    for model in MODELS {
        assert!(listed(&restarted, model), "restart hid {model}");
    }
}
