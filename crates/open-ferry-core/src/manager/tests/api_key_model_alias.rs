// Ported from CLIProxyAPI sdk/cliproxy/auth/api_key_model_alias_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Model aliases of configured API keys: the lookup by alias or upstream
//! name (case-insensitive, keeping a request suffix unless the config sets
//! one), each provider's key list, settings reloads, and the force-mapping
//! alias result.
//!
//! Deviations from upstream:
//! - The port compiles a credential's alias table on each call instead of
//!   caching it per credential ID (a listed deviation of `models.rs`), so
//!   there is no `lookupAPIKeyUpstreamModel(authID, model)`. The
//!   `lookup_upstream_model` helper here stands in for it: it finds
//!   the registered credential by its trimmed ID (none: empty) and runs the
//!   port's per-credential lookup on the trimmed model.
//! - `TestLookupAPIKeyUpstreamModel_MetaKey` step 2 calls upstream's
//!   `applyAPIKeyModelAliasWithRouting` with an empty cached table to reach
//!   the config fallback. The port has no cached table, so the step checks
//!   `apply_api_key_model_alias` and the config fallback it runs
//!   (`resolve_api_key_config` then `resolve_model_alias_from_config_models`)
//!   directly.

use std::collections::BTreeMap;

use super::support::*;
use crate::auth::Auth;
use crate::manager::models::{self, AliasResult, Resolver, resolve_api_key_config};
use crate::manager::{ApiKeyEntry, ModelAlias, Settings};

fn model(name: &str, alias: &str, force_mapping: bool) -> ModelAlias {
    ModelAlias {
        name: name.to_owned(),
        alias: alias.to_owned(),
        force_mapping,
    }
}

fn key(api_key: &str, base_url: &str, models: Vec<ModelAlias>) -> ApiKeyEntry {
    ApiKeyEntry {
        api_key: api_key.to_owned(),
        base_url: base_url.to_owned(),
        models,
        ..ApiKeyEntry::default()
    }
}

/// Settings with these API keys per provider (`gemini` for upstream's
/// `GeminiKey`, `gemini-interactions` for `InteractionsKey`, and so on).
fn settings(keys: Vec<(&str, Vec<ApiKeyEntry>)>) -> Settings {
    Settings {
        api_keys: keys
            .into_iter()
            .map(|(provider, entries)| (provider.to_owned(), entries))
            .collect::<BTreeMap<_, _>>(),
        ..Settings::default()
    }
}

fn auth_with(id: &str, provider: &str, attributes: &[(&str, &str)]) -> Auth {
    let mut credential = auth(id, provider);
    for (name, value) in attributes {
        credential
            .attributes
            .insert((*name).to_owned(), (*value).to_owned());
    }
    credential
}

/// Runs `f` with a resolver over the manager's current settings.
fn with_resolver<T>(h: &Harness, f: impl FnOnce(Resolver<'_>) -> T) -> T {
    let (settings, oauth) = h.manager.resolver_parts();
    f(Resolver {
        settings: &settings,
        oauth: &oauth,
    })
}

/// Upstream's `mgr.lookupAPIKeyUpstreamModel(authID, model)`; see the module
/// docs.
fn lookup_upstream_model(h: &Harness, auth_id: &str, requested: &str) -> String {
    let (auth_id, requested) = (auth_id.trim(), requested.trim());
    if auth_id.is_empty() || requested.is_empty() {
        return String::new();
    }
    let Some(credential) = h.manager.get(auth_id) else {
        return String::new();
    };
    with_resolver(h, |resolver| {
        resolver
            .lookup_api_key_upstream_model(&credential, requested)
            .unwrap_or_default()
    })
}

fn resolve_api_key_model_alias_with_result(
    h: &Harness,
    credential: &Auth,
    requested: &str,
) -> AliasResult {
    with_resolver(h, |resolver| {
        resolver.resolve_api_key_model_alias_with_result(credential, requested)
    })
}

#[tokio::test(start_paused = true)]
async fn lookup_api_key_upstream_model() {
    let h = Harness::new(settings(vec![(
        "gemini",
        vec![key(
            "k",
            "https://example.com",
            vec![
                model("gemini-2.5-pro-exp-03-25", "g25p", false),
                model("gemini-2.5-flash(low)", "g25f", false),
            ],
        )],
    )]));
    h.add(
        auth_with(
            "a1",
            "gemini",
            &[("api_key", "k"), ("base_url", "https://example.com")],
        ),
        &[],
    );

    let tests = [
        // Fast path + suffix preservation
        (
            "alias with suffix",
            "a1",
            "g25p(8192)",
            "gemini-2.5-pro-exp-03-25(8192)",
        ),
        (
            "alias without suffix",
            "a1",
            "g25p",
            "gemini-2.5-pro-exp-03-25",
        ),
        // Config suffix takes priority
        (
            "config suffix priority",
            "a1",
            "g25f(high)",
            "gemini-2.5-flash(low)",
        ),
        (
            "config suffix no user suffix",
            "a1",
            "g25f",
            "gemini-2.5-flash(low)",
        ),
        // Case insensitive
        ("uppercase alias", "a1", "G25P", "gemini-2.5-pro-exp-03-25"),
        (
            "mixed case with suffix",
            "a1",
            "G25p(4096)",
            "gemini-2.5-pro-exp-03-25(4096)",
        ),
        // Direct name lookup
        (
            "upstream name direct",
            "a1",
            "gemini-2.5-pro-exp-03-25",
            "gemini-2.5-pro-exp-03-25",
        ),
        (
            "upstream name with suffix",
            "a1",
            "gemini-2.5-pro-exp-03-25(8192)",
            "gemini-2.5-pro-exp-03-25(8192)",
        ),
        // Cache miss scenarios
        ("non-existent auth", "non-existent", "g25p", ""),
        ("unknown alias", "a1", "unknown-alias", ""),
        ("empty auth ID", "", "g25p", ""),
        ("empty model", "a1", "", ""),
    ];

    for (name, auth_id, input, want) in tests {
        let resolved = lookup_upstream_model(&h, auth_id, input);
        assert_eq!(
            resolved, want,
            "{name}: lookupAPIKeyUpstreamModel({auth_id:?}, {input:?})"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn lookup_api_key_upstream_model_interactions_key() {
    let h = Harness::new(settings(vec![(
        "gemini-interactions",
        vec![key(
            "interactions-key",
            "https://interactions.example.com",
            vec![model("gemini-2.5-flash", "native-flash", false)],
        )],
    )]));
    h.add(
        auth_with(
            "interactions-auth",
            "gemini-interactions",
            &[
                ("api_key", "interactions-key"),
                ("base_url", "https://interactions.example.com"),
            ],
        ),
        &[],
    );

    let resolved = lookup_upstream_model(&h, "interactions-auth", "native-flash");
    assert_eq!(resolved, "gemini-2.5-flash", "lookupAPIKeyUpstreamModel()");
}

#[tokio::test(start_paused = true)]
async fn api_key_model_alias_config_hot_reload() {
    let h = Harness::new(settings(vec![(
        "gemini",
        vec![key(
            "k",
            "",
            vec![model("gemini-2.5-pro-exp-03-25", "g25p", false)],
        )],
    )]));
    h.add(auth_with("a1", "gemini", &[("api_key", "k")]), &[]);

    // Initial alias
    assert_eq!(
        lookup_upstream_model(&h, "a1", "g25p"),
        "gemini-2.5-pro-exp-03-25",
        "before reload"
    );

    // Hot reload with new alias
    h.manager.set_settings(settings(vec![(
        "gemini",
        vec![key("k", "", vec![model("gemini-2.5-flash", "g25p", false)])],
    )]));

    // New alias should take effect
    assert_eq!(
        lookup_upstream_model(&h, "a1", "g25p"),
        "gemini-2.5-flash",
        "after reload"
    );
}

#[tokio::test(start_paused = true)]
async fn api_key_model_alias_multiple_providers() {
    let h = Harness::new(settings(vec![
        (
            "gemini",
            vec![key(
                "gemini-key",
                "",
                vec![model("gemini-2.5-pro", "gp", false)],
            )],
        ),
        (
            "claude",
            vec![key(
                "claude-key",
                "",
                vec![model("claude-sonnet-4", "cs4", false)],
            )],
        ),
        (
            "codex",
            vec![key("codex-key", "", vec![model("o3", "o", false)])],
        ),
        (
            "xai",
            vec![key(
                "xai-key",
                "",
                vec![model("grok-4.5", "grok-latest", false)],
            )],
        ),
    ]));
    h.add(
        auth_with("gemini-auth", "gemini", &[("api_key", "gemini-key")]),
        &[],
    );
    h.add(
        auth_with("claude-auth", "claude", &[("api_key", "claude-key")]),
        &[],
    );
    h.add(
        auth_with("codex-auth", "codex", &[("api_key", "codex-key")]),
        &[],
    );
    h.add(auth_with("xai-auth", "xai", &[("api_key", "xai-key")]), &[]);

    let tests = [
        ("gemini-auth", "gp", "gemini-2.5-pro"),
        ("claude-auth", "cs4", "claude-sonnet-4"),
        ("codex-auth", "o", "o3"),
        ("xai-auth", "grok-latest", "grok-4.5"),
    ];
    for (auth_id, input, want) in tests {
        assert_eq!(
            lookup_upstream_model(&h, auth_id, input),
            want,
            "lookupAPIKeyUpstreamModel({auth_id:?}, {input:?})"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn apply_api_key_model_alias() {
    let h = Harness::new(settings(vec![(
        "gemini",
        vec![key(
            "k",
            "",
            vec![model("gemini-2.5-pro-exp-03-25", "g25p", false)],
        )],
    )]));
    let api_key_auth = auth_with("a1", "gemini", &[("api_key", "k")]);
    let oauth_auth = auth_with("oauth-auth", "claude", &[("auth_kind", "oauth")]);
    h.add(api_key_auth.clone(), &[]);

    let tests = [
        (
            "api_key auth with alias",
            &api_key_auth,
            "g25p(8192)",
            "gemini-2.5-pro-exp-03-25(8192)",
        ),
        (
            "oauth auth passthrough",
            &oauth_auth,
            "some-model",
            "some-model",
        ),
    ];
    for (name, credential, input_model, want_model) in tests {
        let resolved_model = with_resolver(&h, |resolver| {
            resolver.apply_api_key_model_alias(credential, input_model)
        });
        assert_eq!(resolved_model, want_model, "{name}: model");
    }
}

#[tokio::test(start_paused = true)]
async fn resolve_api_key_model_alias_with_result_force_mapping() {
    let h = Harness::new(settings(vec![(
        "claude",
        vec![key(
            "claude-key",
            "",
            vec![model("glm-5.2", "claude-sonnet-latest", true)],
        )],
    )]));
    let credential = h.add(
        auth_with("claude-auth", "claude", &[("api_key", "claude-key")]),
        &[],
    );

    let result = resolve_api_key_model_alias_with_result(&h, &credential, "claude-sonnet-latest");
    assert!(
        result.upstream_model == "glm-5.2"
            && result.force_mapping
            && result.original_alias == "claude-sonnet-latest",
        "resolveAPIKeyModelAliasWithResult() = {result:?}, want upstream glm-5.2 with force mapping"
    );

    let no_rewrite = resolve_api_key_model_alias_with_result(&h, &credential, "glm-5.2");
    assert!(
        no_rewrite.upstream_model == "glm-5.2"
            && !no_rewrite.force_mapping
            && no_rewrite.original_alias.is_empty(),
        "resolveAPIKeyModelAliasWithResult() direct upstream = {no_rewrite:?}, want passthrough without rewrite"
    );
}

#[tokio::test(start_paused = true)]
async fn resolve_api_key_model_alias_with_result_same_base_preserves_suffix() {
    let h = Harness::new(settings(vec![(
        "gemini",
        vec![key(
            "k",
            "",
            vec![model("gemini-2.5-pro", "gemini-2.5-pro(8192)", true)],
        )],
    )]));
    let credential = h.add(auth_with("gemini-auth", "gemini", &[("api_key", "k")]), &[]);

    let result = resolve_api_key_model_alias_with_result(&h, &credential, "gemini-2.5-pro(8192)");
    assert!(
        result.upstream_model == "gemini-2.5-pro(8192)"
            && result.force_mapping
            && result.original_alias == "gemini-2.5-pro(8192)",
        "resolveAPIKeyModelAliasWithResult() = {result:?}, want same-base suffix preserved"
    );
}

#[tokio::test(start_paused = true)]
async fn resolve_api_key_model_alias_with_result_force_mapping_uses_config_alias_not_request_suffix()
 {
    let h = Harness::new(settings(vec![(
        "codex",
        vec![key(
            "codex-key",
            "",
            vec![model("gpt-5.5", "claude-sonnet-4-5", true)],
        )],
    )]));
    let credential = h.add(
        auth_with("codex-auth", "codex", &[("api_key", "codex-key")]),
        &[],
    );

    let result =
        resolve_api_key_model_alias_with_result(&h, &credential, "claude-sonnet-4-5(high)");
    assert_eq!(result.upstream_model, "gpt-5.5(high)", "upstream");
    assert_eq!(result.original_alias, "claude-sonnet-4-5", "OriginalAlias");
}

#[tokio::test(start_paused = true)]
async fn lookup_api_key_upstream_model_meta_key() {
    let h = Harness::new(settings(vec![(
        "meta",
        vec![key(
            "meta-key",
            "https://api.meta.ai/v1",
            vec![model("muse-spark-1.3", "muse-latest", false)],
        )],
    )]));
    let credential = h.add(
        auth_with(
            "meta-auth-1",
            "meta",
            &[
                ("api_key", "meta-key"),
                ("base_url", "https://api.meta.ai/v1"),
                ("auth_kind", "apikey"),
            ],
        ),
        &[],
    );

    // 1. Fast path: the credential's alias table.
    let resolved = lookup_upstream_model(&h, "meta-auth-1", "muse-latest");
    assert_eq!(resolved, "muse-spark-1.3", "lookupAPIKeyUpstreamModel()");

    // 2. Slow path: the config fallback (see the module docs).
    let slow_resolved = with_resolver(&h, |resolver| {
        resolver.apply_api_key_model_alias(&credential, "muse-latest")
    });
    assert_eq!(
        slow_resolved, "muse-spark-1.3",
        "applyAPIKeyModelAliasWithRouting(slow)"
    );
    let (current, _) = h.manager.resolver_parts();
    let entry = resolve_api_key_config(current.api_key_entries("meta"), &credential)
        .expect("meta key config");
    assert_eq!(
        models::resolve_model_alias_from_config_models("muse-latest", &entry.models),
        "muse-spark-1.3",
        "config fallback"
    );

    // 3. Model alias result with force mapping / alias metadata
    let alias_result = resolve_api_key_model_alias_with_result(&h, &credential, "muse-latest");
    assert_eq!(
        alias_result.upstream_model, "muse-spark-1.3",
        "resolveAPIKeyModelAliasWithResult() upstream"
    );

    // 4. Configured alias entries helper
    let entries = with_resolver(&h, |resolver| {
        resolver
            .configured_model_alias_entries(&credential)
            .to_vec()
    });
    assert!(
        entries
            .iter()
            .any(|e| e.alias == "muse-latest" && e.name == "muse-spark-1.3"),
        "configuredModelAliasEntries did not contain muse-latest -> muse-spark-1.3: {entries:?}"
    );
}
