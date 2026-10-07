// Ported from CLIProxyAPI sdk/cliproxy/auth/scheduler_test.go
// (TestManagerCodexAlphaSearchPolicyFiltersBeforePluginScheduler,
// TestManagerCodexAlphaSearchPolicyRejectsOrdinaryAPIKey) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The `codex_alpha_search_v1` credential policy, the pick that honours
//! it, and the upstream model a picked credential is sent.
//!
//! Deviations from upstream:
//! - FiltersBeforePluginScheduler: the plugin scheduler isn't ported, so
//!   the test checks that round robin never reaches the credential the
//!   policy leaves out, where upstream checks the candidates its plugin was
//!   offered.
//! - Dropped: TestSelectHomeAuthWithCredentialPolicyTransportsAndValidatesPolicy
//!   (the Home dispatcher isn't ported).
//! - The other tests aren't upstream's; upstream covers the policy and
//!   `ResolveExecutionModel` through its Alpha Search handler tests, which
//!   the server crate ports.

use serde_json::json;

use super::support::*;
use crate::auth::Auth;
use crate::exec::{ErrorKind, ExecError};
use crate::manager::policy::CredentialPolicy;
use crate::manager::{ApiKeyEntry, ModelAlias, RoutingStrategy, Settings};

/// A credential with `attributes`.
fn cred(id: &str, provider: &str, attributes: &[(&str, &str)]) -> Auth {
    let mut auth = auth(id, provider);
    for (key, value) in attributes {
        auth.attributes
            .insert((*key).to_owned(), (*value).to_owned());
    }
    auth
}

/// A manager with `settings` and a fake Codex executor.
fn harness(settings: Settings) -> Harness {
    let h = Harness::new(settings);
    h.executor(&FakeExecutor::new("codex"));
    h
}

/// The credential Codex Alpha Search picks for `model`.
fn pick(h: &Harness, model: &str) -> Result<String, ExecError> {
    h.manager
        .select_auth_with_credential_policy(
            "codex",
            model,
            CredentialPolicy::CodexAlphaSearchV1,
            None,
        )
        .map(|picked| picked.auth.id.clone())
}

// Ports TestManagerCodexAlphaSearchPolicyFiltersBeforePluginScheduler.
#[tokio::test(start_paused = true)]
async fn manager_codex_alpha_search_policy_filters_before_the_selector() {
    let h = harness(Settings::default());
    h.add(
        cred("ordinary-api-key", "codex", &[("api_key", "ordinary")]),
        &[],
    );
    h.add(
        cred(
            "alpha-api-key",
            "codex",
            &[
                ("api_key", "alpha"),
                ("codex_alpha_search", "true"),
                ("base_url", "https://codex.example.com/v1"),
            ],
        ),
        &[],
    );
    for _ in 0..3 {
        assert_eq!(pick(&h, "").unwrap(), "alpha-api-key");
    }
}

// Ports TestManagerCodexAlphaSearchPolicyRejectsOrdinaryAPIKey.
#[tokio::test(start_paused = true)]
async fn manager_codex_alpha_search_policy_rejects_ordinary_api_key() {
    let h = harness(Settings::default());
    h.add(
        cred("ordinary-api-key", "codex", &[("api_key", "ordinary")]),
        &[],
    );
    let err = pick(&h, "").unwrap_err();
    assert_eq!(err.kind, ErrorKind::AuthNotFound, "{err}");
    assert_eq!(err.to_string(), "auth_not_found: no auth available");
}

// Not upstream's: which credentials the policy allows.
#[test]
fn codex_alpha_search_policy_allows_oauth_and_opted_in_api_keys() {
    let policy = CredentialPolicy::CodexAlphaSearchV1;
    let oauth = auth_with_metadata("oauth", " Codex ", json!({"access_token": "t"}));
    assert!(policy.allows(&oauth));
    let opted_in = cred(
        "key",
        "codex",
        &[("api_key", "k"), ("codex_alpha_search", " TRUE ")],
    );
    assert!(policy.allows(&opted_in));
    for (attribute, value) in [("codex_alpha_search", "false"), ("codex_alpha_search", "1")] {
        let key = cred("key", "codex", &[("api_key", "k"), (attribute, value)]);
        assert!(!policy.allows(&key), "{attribute}={value}");
    }
    assert!(!policy.allows(&cred("key", "codex", &[("api_key", "k")])));
    let other = auth_with_metadata("other", "claude", json!({"access_token": "t"}));
    assert!(!policy.allows(&other));
    assert!(!policy.allows(&auth("bare", "codex")));
}

// Not upstream's: the policy pick checks the route model, rotates within
// the provider, and needs the provider's executor.
#[tokio::test(start_paused = true)]
async fn codex_alpha_search_pick_checks_the_model_and_rotates() {
    let h = harness(Settings {
        routing_strategy: RoutingStrategy::RoundRobin,
        ..Settings::default()
    });
    for id in ["oauth-a", "oauth-b", "oauth-c"] {
        let models: &[&str] = if id == "oauth-c" {
            &["other"]
        } else {
            &["gpt-5.6-sol"]
        };
        h.add(
            auth_with_metadata(id, "codex", json!({"access_token": id})),
            models,
        );
    }
    let picks: Vec<String> = (0..3).map(|_| pick(&h, "gpt-5.6-sol").unwrap()).collect();
    assert_eq!(picks, ["oauth-a", "oauth-b", "oauth-a"]);
    assert_eq!(pick(&h, "gpt-5.6-sol(high)").unwrap(), "oauth-b");
    assert_eq!(
        pick(&h, "missing").unwrap_err().to_string(),
        "auth_not_found: no auth available"
    );

    let bare = Harness::new(Settings::default());
    bare.add(
        auth_with_metadata("oauth", "codex", json!({"access_token": "t"})),
        &[],
    );
    let err = pick(&bare, "").unwrap_err();
    assert_eq!(err.kind, ErrorKind::ExecutorNotFound, "{err}");
}

// Not upstream's: the model a credential is sent, without its prefix and
// through its API key's aliases.
#[tokio::test(start_paused = true)]
async fn resolves_the_execution_model_for_a_credential() {
    let h = harness(Settings {
        api_keys: [(
            "codex".to_owned(),
            vec![ApiKeyEntry {
                api_key: "codex-alpha-key".into(),
                base_url: "https://codex.example.com/v1".into(),
                prefix: "vendor".into(),
                models: vec![ModelAlias {
                    name: "gpt-5.6-sol".into(),
                    alias: "sol-alias".into(),
                    force_mapping: false,
                }],
                ..ApiKeyEntry::default()
            }],
        )]
        .into(),
        ..Settings::default()
    });
    let mut key = cred(
        "key",
        "codex",
        &[
            ("api_key", "codex-alpha-key"),
            ("base_url", "https://codex.example.com/v1"),
        ],
    );
    key.prefix = "vendor".into();
    let m = &h.manager;
    assert_eq!(
        m.resolve_execution_model(&key, " vendor/sol-alias "),
        "gpt-5.6-sol"
    );
    assert_eq!(
        m.resolve_execution_model(&key, "vendor/gpt-5.6-sol"),
        "gpt-5.6-sol"
    );
    assert_eq!(m.resolve_execution_model(&key, "other/x"), "other/x");
    assert_eq!(m.resolve_execution_model(&key, "  "), "");
}
