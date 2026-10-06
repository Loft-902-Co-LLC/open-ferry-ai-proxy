// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_executor_replace_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Registering executors: a replaced executor's sessions close, lookups
//! ignore case and resolve the Kimi aliases, and Kimi's two domains pick
//! only their own credentials.
//!
//! Deviations from upstream:
//! - The port has one pick path (`pick_next_mixed`) where upstream has
//!   `pickNext`, `pickNextLegacy` and `pickNextMixed`; each upstream pick is
//!   made through it, with the provider as a one-element list.
//! - `HasProviderAuth` isn't ported (its one caller upstream is the plugin
//!   host's model router), so its assertions are dropped; the picks beside
//!   them are kept.
//! - Upstream's `refreshAuthForRequest(id, "")` is `force_refresh(id)`.

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::json;

use super::support::*;
use crate::auth::{Auth, Status};
use crate::exec::{Dispatcher, ExecError};
use crate::executor::ProviderExecutor;
use crate::manager::Settings;
use crate::manager::models::Resolver;
use crate::manager::select::{
    CLOSE_ALL_EXECUTION_SESSIONS, PickArgs, Picked, Selection, normalize_providers,
};

/// Picks the next credential for `names` and `model` as a call would
/// (upstream's `pickNext`, `pickNextLegacy` and `pickNextMixed`).
fn pick(h: &Harness, names: &[&str], model: &str, pinned_id: &str) -> Result<Picked, ExecError> {
    let now = h.now();
    let mut guard = h.manager.lock();
    let state = &mut *guard;
    let settings = state.settings.clone();
    let oauth = state.oauth.clone();
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
        pinned: pinned_id,
        downstream_websocket: false,
        eligibility: Default::default(),
        tried: &tried,
    };
    let normalized = normalize_providers(&providers(names));
    selection.pick_next_mixed(&mut state.selector, &normalized, &args)
}

fn same_executor(got: &Arc<dyn ProviderExecutor>, want: &Arc<FakeExecutor>) -> bool {
    std::ptr::addr_eq(Arc::as_ptr(got), Arc::as_ptr(want))
}

fn kimi_auth(id: &str, provider: &str, token: &str) -> Auth {
    let mut auth = auth_with_metadata(id, provider, json!({"access_token": token}));
    auth.status = Status::Active;
    auth
}

#[tokio::test(start_paused = true)]
async fn manager_register_executor_closes_replaced_execution_sessions() {
    let h = Harness::new(Settings::default());
    let replaced = FakeExecutor::new("codex");
    let current = FakeExecutor::new("codex");

    h.executor(&replaced);
    h.executor(&current);

    let closed = replaced.closed_sessions();
    assert_eq!(closed.len(), 1, "replaced executor close calls");
    assert_eq!(closed[0], CLOSE_ALL_EXECUTION_SESSIONS);
    assert!(
        current.closed_sessions().is_empty(),
        "expected current executor to stay open"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_executor_returns_registered_executor() {
    let h = Harness::new(Settings::default());
    let current = FakeExecutor::new("codex");
    h.executor(&current);

    let resolved = h
        .manager
        .executor("CODEX")
        .expect("expected registered executor to be found");
    assert!(
        same_executor(&resolved, &current),
        "expected resolved executor to match registered executor"
    );

    assert!(
        h.manager.executor("unknown").is_none(),
        "expected unknown provider lookup to fail"
    );

    let kimi = FakeExecutor::new("kimi");
    h.executor(&kimi);
    for provider in ["kimi", "KIMI", "kimi-ai", "kimi.ai", "kimi.com"] {
        let resolved = h
            .manager
            .executor(provider)
            .unwrap_or_else(|| panic!("expected executor for {provider:?} to be found"));
        assert!(
            same_executor(&resolved, &kimi),
            "expected resolved executor for {provider:?} to match kimi"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn manager_refresh_and_legacy_selection_kimi_aliases() {
    let h = Harness::new(Settings::default());
    let kimi = FakeExecutor::new("kimi");
    kimi.set_refresh(|auth| {
        let mut updated = auth.clone();
        updated.metadata.insert("refreshed".into(), json!(true));
        Ok(updated)
    });
    h.executor(&kimi);

    for provider in ["kimi", "kimi-ai", "kimi.ai", "kimi.com"] {
        let auth_id = format!("auth-{provider}");
        h.manager
            .register(kimi_auth(&auth_id, provider, &format!("token-{provider}")))
            .unwrap_or_else(|err| panic!("register({provider}) error = {err}"));

        let refreshed = h
            .manager
            .force_refresh(&auth_id)
            .await
            .unwrap_or_else(|err| panic!("refresh({provider}) error = {err}"));
        assert_eq!(
            refreshed.metadata.get("refreshed"),
            Some(&json!(true)),
            "expected refreshed metadata for provider {provider}"
        );

        let picked = pick(&h, &[provider], "", "")
            .unwrap_or_else(|err| panic!("pick({provider}) error = {err}"));
        assert!(
            same_executor(&picked.executor, &kimi),
            "expected the kimi executor for provider {provider}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn manager_scheduler_fast_path_kimi_ai() {
    let h = Harness::new(Settings::default());
    let kimi = FakeExecutor::new("kimi");
    h.executor(&kimi);
    let auth_id = "auth-kimi-ai-fastpath";
    h.add(kimi_auth(auth_id, "kimi-ai", "token-ai"), &["kimi-k2"]);

    // 1. Single provider pick.
    let picked = pick(&h, &["kimi-ai"], "", "").expect("pick(kimi-ai)");
    assert_eq!(picked.auth.id, auth_id);
    assert!(same_executor(&picked.executor, &kimi), "pick(kimi-ai) exec");

    // 2. Mixed provider pick.
    let mixed = pick(&h, &["kimi-ai"], "", "").expect("pick mixed([kimi-ai])");
    assert_eq!(mixed.auth.id, auth_id);
    assert!(same_executor(&mixed.executor, &kimi), "pick mixed exec");
    assert_eq!(mixed.provider, "kimi-ai");

    // 3. Pinned pick.
    let pinned_pick = pick(&h, &["kimi-ai"], "", auth_id).expect("pick pinned");
    assert_eq!(pinned_pick.auth.id, auth_id);

    // 4. Non-streaming call.
    h.manager
        .execute(&providers(&["kimi-ai"]), request("kimi-k2"), options())
        .await
        .expect("manager.execute([kimi-ai])");

    // 5. Streaming call.
    h.manager
        .execute_stream(&providers(&["kimi-ai"]), request("kimi-k2"), options())
        .await
        .expect("manager.execute_stream([kimi-ai])");
}

#[tokio::test(start_paused = true)]
async fn manager_kimi_domain_isolation() {
    let auth_com = kimi_auth("auth-kimi-com-domain", "kimi", "token-com");
    let auth_ai = kimi_auth("auth-kimi-ai-domain", "kimi-ai", "token-ai");

    let h = Harness::new(Settings::default());
    let kimi = FakeExecutor::new("kimi");
    h.executor(&kimi);
    h.add(auth_com.clone(), &["kimi-k2"]);
    h.add(auth_ai.clone(), &["kimi-k2"]);

    // kimi (or kimi.com) picks only the .com credential.
    let picked_com = pick(&h, &["kimi"], "kimi-k2", "").expect("pick(kimi)");
    assert_eq!(picked_com.auth.id, auth_com.id);

    // kimi-ai (or kimi.ai) picks only the .ai credential.
    let picked_ai = pick(&h, &["kimi-ai"], "kimi-k2", "").expect("pick(kimi-ai)");
    assert_eq!(picked_ai.auth.id, auth_ai.id);

    // Each domain stays isolated when the other has no credential.
    let only_com = Harness::new(Settings::default());
    only_com.executor(&kimi);
    only_com.add(auth_com, &["kimi-k2"]);
    assert!(
        pick(&only_com, &["kimi-ai"], "kimi-k2", "").is_err(),
        "expected error picking kimi-ai on com-only manager"
    );

    let only_ai = Harness::new(Settings::default());
    only_ai.executor(&kimi);
    only_ai.add(auth_ai, &["kimi-k2"]);
    assert!(
        pick(&only_ai, &["kimi"], "kimi-k2", "").is_err(),
        "expected error picking kimi on ai-only manager"
    );
}
