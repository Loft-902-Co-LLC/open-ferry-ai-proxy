// Ported from CLIProxyAPI sdk/api/handlers/openai/openai_responses_websocket_test.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! [`Dispatcher::websocket_support`] as the manager answers it: which
//! credentials may serve a model over the Responses WebSocket.
//!
//! Deviations from upstream:
//! - Upstream's tests call the handler's lookups, which read the global
//!   auth manager and registry; these ask the manager, with the providers
//!   the handler would have routed the model to.
//! - `TestWebsocketUpstreamSupportsIncrementalInputForModel` and `...ForXAI`
//!   test a lookup only upstream's tests use; here they check the asked-about
//!   credential's `websockets` flag instead.
//! - The Home runtime cases of `TestResponsesWebsocketPinnedAuthMatchesModel`
//!   are dropped: the Home runtime isn't ported.

use std::time::Duration;

use chrono::TimeDelta;

use super::support::*;
use crate::auth::{Auth, ModelState, Status};
use crate::exec::{Dispatcher, WebsocketSupport};
use crate::manager::Settings;

fn providers(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn ws_auth(id: &str, provider: &str) -> Auth {
    let mut auth = auth(id, provider);
    auth.status = Status::Active;
    auth.attributes
        .insert("websockets".to_owned(), "true".to_owned());
    auth
}

fn support(h: &Harness, providers: &[String], model: &str, id: Option<&str>) -> WebsocketSupport {
    h.manager.websocket_support(providers, model, id)
}

// TestResponsesWebsocketPinnedAuthMatchesModel
#[test]
fn pinned_auth_matches_model() {
    let h = Harness::new(Settings::default());
    let model_a = "xai-pinned-auth-model-a";
    let model_b = "xai-pinned-auth-model-b";
    let mut base = auth("xai-pinned-auth", "xai");
    base.status = Status::Active;
    h.add(base.clone(), &[model_a]);
    h.models.register("xai-pinned-auth-other", &[model_b]);
    let xai = providers(&["xai"]);

    let found = support(&h, &xai, model_a, Some("xai-pinned-auth")).auth;
    assert!(found.as_ref().is_some_and(|auth| auth.serves_model));
    assert_eq!(found.map(|auth| auth.provider).as_deref(), Some("xai"));
    let found = support(&h, &xai, model_b, Some("xai-pinned-auth")).auth;
    assert!(
        found.is_some_and(|auth| !auth.serves_model),
        "registered auth matched an unsupported model from the same provider"
    );

    let mut disabled = base.clone();
    disabled.disabled = true;
    h.add(disabled, &[model_a]);
    let found = support(&h, &xai, model_a, Some("xai-pinned-auth")).auth;
    assert!(
        found.is_some_and(|auth| !auth.serves_model),
        "disabled auth matched a model"
    );

    let mut cooling = base.clone();
    cooling.model_states.insert(
        model_a.to_owned(),
        ModelState {
            unavailable: true,
            next_retry_after: Some(h.now() + TimeDelta::seconds(60)),
            ..ModelState::default()
        },
    );
    h.add(cooling, &[model_a]);
    let found = support(&h, &xai, model_a, Some("xai-pinned-auth")).auth;
    assert!(
        found.is_some_and(|auth| !auth.serves_model),
        "auth in model cooldown matched a model"
    );

    let mut unregistered = auth("unregistered-auth", "xai");
    unregistered.status = Status::Active;
    h.add(unregistered, &[]);
    let found = support(&h, &xai, model_a, Some("unregistered-auth")).auth;
    assert!(
        found.is_some_and(|auth| !auth.serves_model),
        "unregistered ordinary auth matched a model"
    );
}

// Not upstream's: a cooldown that has passed, a cooldown on the base model of
// a suffixed one, and a provider not asked about.
#[test]
fn pinned_auth_cooldown_edges() {
    let h = Harness::new(Settings::default());
    let mut auth = ws_auth("codex-a", "Codex");
    auth.model_states.insert(
        "gpt-5".to_owned(),
        ModelState {
            unavailable: true,
            next_retry_after: Some(h.now() + TimeDelta::seconds(60)),
            ..ModelState::default()
        },
    );
    h.add(auth, &["gpt-5", "gpt-5(high)", "gpt-4"]);
    let codex = providers(&["codex"]);

    let serves = |model: &str, providers: &[String]| {
        support(&h, providers, model, Some("codex-a"))
            .auth
            .is_some_and(|auth| auth.serves_model)
    };
    assert!(!serves("gpt-5(high)", &codex));
    // A credential with model states but none for the model falls back to
    // its own availability.
    assert!(serves("gpt-4", &codex));
    assert!(!serves("gpt-4", &providers(&["xai"])));
    h.clock.advance(Duration::from_secs(61));
    assert!(serves("gpt-5(high)", &codex));
    assert!(support(&h, &codex, "gpt-5", Some("missing")).auth.is_none());
}

// TestWebsocketUpstreamSupportsIncrementalInputForModel
#[test]
fn incremental_input_for_model() {
    let h = Harness::new(Settings::default());
    h.add(ws_auth("auth-ws", "test-provider"), &["test-model"]);
    let found = support(
        &h,
        &providers(&["test-provider"]),
        "test-model",
        Some("auth-ws"),
    )
    .auth;
    assert!(found.is_some_and(|auth| auth.websockets && auth.serves_model));
}

// TestWebsocketUpstreamSupportsIncrementalInputForXAI, inverted: xAI's
// WebSocket executor isn't ported, so an xAI credential with websockets on,
// by attribute or metadata, is served over HTTP.
#[test]
fn incremental_input_for_xai() {
    let h = Harness::new(Settings::default());
    h.add(ws_auth("auth-xai-ws", "xai"), &["xai-test-model"]);
    let found = support(
        &h,
        &providers(&["xai"]),
        "xai-test-model",
        Some("auth-xai-ws"),
    )
    .auth;
    assert!(found.is_some_and(|auth| !auth.websockets && auth.serves_model));

    let mut off = ws_auth("auth-xai-off", "xai");
    off.attributes.clear();
    off.metadata
        .insert("websockets".to_owned(), serde_json::Value::from("TRUE"));
    h.add(off, &["xai-test-model"]);
    let found = support(
        &h,
        &providers(&["xai"]),
        "xai-test-model",
        Some("auth-xai-off"),
    )
    .auth;
    assert!(
        found.is_some_and(|auth| !auth.websockets),
        "xAI is served over HTTP"
    );
}

// TestResponsesWebsocketUsesUpstreamWebsocketPassthroughForXAI, inverted:
// xAI's WebSocket executor isn't ported, so there is no passthrough.
#[test]
fn upstream_passthrough_for_xai() {
    let h = Harness::new(Settings::default());
    h.executor(&FakeExecutor::new("xai"));
    let model = "xai-passthrough-model";
    h.add(ws_auth("auth-xai-ws", "xai"), &[model]);
    let support = support(&h, &providers(&["xai"]), model, None);
    assert!(!support.upstream_passthrough);
    assert!(!support.compaction_replay);
}

// Not upstream's: passthrough needs a registered executor, websockets on for
// every credential, one provider, and a model.
#[test]
fn upstream_passthrough_requirements() {
    let h = Harness::new(Settings::default());
    let model = "gpt-5";
    let codex = providers(&["codex"]);
    h.add(ws_auth("codex-a", "codex"), &[model]);
    assert!(
        !support(&h, &codex, model, None).upstream_passthrough,
        "no executor registered"
    );
    h.executor(&FakeExecutor::new("codex"));
    assert!(support(&h, &codex, model, None).upstream_passthrough);
    assert!(!support(&h, &codex, "", None).upstream_passthrough);
    assert!(!support(&h, &[], model, None).upstream_passthrough);

    let mut plain = ws_auth("codex-b", "codex");
    plain.attributes.clear();
    h.add(plain, &[model]);
    assert!(
        !support(&h, &codex, model, None).upstream_passthrough,
        "a credential without websockets"
    );
    h.manager.remove("codex-b");

    h.executor(&FakeExecutor::new("xai"));
    h.add(ws_auth("xai-a", "xai"), &[model]);
    assert!(
        !support(&h, &providers(&["codex", "xai"]), model, None).upstream_passthrough,
        "mixed providers"
    );
    assert!(support(&h, &codex, model, None).upstream_passthrough);
}

// TestWebsocketUpstreamSupportsCompactionReplayForModel
#[test]
fn compaction_replay_for_model() {
    let h = Harness::new(Settings::default());
    let mut codex = auth("auth-codex", "codex");
    codex.status = Status::Active;
    h.add(codex, &["test-model"]);
    assert!(support(&h, &providers(&["codex"]), "test-model", None).compaction_replay);
}

// TestWebsocketUpstreamSupportsCompactionReplayForModelFalseWhenMixedBackends
#[test]
fn compaction_replay_false_when_mixed_backends() {
    let h = Harness::new(Settings::default());
    for (id, provider) in [("auth-codex", "codex"), ("auth-claude", "claude")] {
        let mut auth = auth(id, provider);
        auth.status = Status::Active;
        h.add(auth, &["test-model"]);
    }
    let both = providers(&["codex", "claude"]);
    assert!(!support(&h, &both, "test-model", None).compaction_replay);
    assert!(!support(&h, &providers(&["gemini"]), "test-model", None).compaction_replay);
}
