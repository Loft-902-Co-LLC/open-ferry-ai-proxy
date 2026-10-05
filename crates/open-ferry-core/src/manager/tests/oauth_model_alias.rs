// Ported from CLIProxyAPI sdk/cliproxy/auth/oauth_model_alias_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! OAuth model aliases: which upstream model an alias resolves to, how the
//! request's thinking suffix carries over, per-credential aliases, and which
//! providers and credential kinds have an alias channel.
//!
//! Deviations from upstream:
//! - The aliases are set through `Manager::set_settings` (upstream's
//!   `SetOAuthModelAlias`) and resolved through the manager's compiled table.
//! - Upstream's `Fork` flag is left out of the alias entries: it only adds
//!   the alias to model listings and doesn't change resolution, and the
//!   port's `ModelAlias` has no such field.

use std::collections::BTreeMap;

use super::support::*;
use crate::auth::Auth;
use crate::manager::models::{Resolver, oauth_model_alias_channel};
use crate::manager::{ModelAlias, Settings};

fn alias(name: &str, alias: &str) -> ModelAlias {
    ModelAlias {
        name: name.to_owned(),
        alias: alias.to_owned(),
        force_mapping: false,
    }
}

fn forced(name: &str, alias: &str) -> ModelAlias {
    ModelAlias {
        force_mapping: true,
        ..self::alias(name, alias)
    }
}

/// Aliases by channel, as upstream's `SetOAuthModelAlias` takes them.
type ChannelAliases<'a> = Vec<(&'a str, Vec<ModelAlias>)>;

/// A manager with `aliases` as its OAuth model aliases.
fn manager_with(aliases: ChannelAliases) -> Harness {
    let h = Harness::new(Settings::default());
    let oauth_model_alias: BTreeMap<String, Vec<ModelAlias>> = aliases
        .into_iter()
        .map(|(channel, entries)| (channel.to_owned(), entries))
        .collect();
    h.manager.set_settings(Settings {
        oauth_model_alias,
        ..Settings::default()
    });
    h
}

/// Runs `f` with the manager's current settings and alias table.
fn with_resolver<R>(h: &Harness, f: impl FnOnce(Resolver<'_>) -> R) -> R {
    let (settings, oauth) = h.manager.resolver_parts();
    f(Resolver {
        settings: &settings,
        oauth: &oauth,
    })
}

fn with_attributes(id: &str, provider: &str, attributes: &[(&str, &str)]) -> Auth {
    let mut auth = auth(id, provider);
    for (key, value) in attributes {
        auth.attributes
            .insert((*key).to_owned(), (*value).to_owned());
    }
    auth
}

/// Upstream's `createAuthForChannel`.
fn create_auth_for_channel(channel: &str) -> Auth {
    match channel {
        "antigravity" | "claude" | "vertex" | "codex" | "meta" => {
            with_attributes("", channel, &[("auth_kind", "oauth")])
        }
        _ => auth("", channel),
    }
}

#[test]
fn resolve_oauth_upstream_model_suffix_preservation() {
    let gemini = || alias("gemini-2.5-pro-exp-03-25", "gemini-2.5-pro");
    let cases: Vec<(&str, ChannelAliases, &str, &str, &str)> = vec![
        (
            "numeric suffix preserved",
            vec![("antigravity", vec![gemini()])],
            "antigravity",
            "gemini-2.5-pro(8192)",
            "gemini-2.5-pro-exp-03-25(8192)",
        ),
        (
            "level suffix preserved",
            vec![(
                "claude",
                vec![alias("claude-sonnet-4-5-20250514", "claude-sonnet-4-5")],
            )],
            "claude",
            "claude-sonnet-4-5(high)",
            "claude-sonnet-4-5-20250514(high)",
        ),
        (
            "no suffix unchanged",
            vec![("antigravity", vec![gemini()])],
            "antigravity",
            "gemini-2.5-pro",
            "gemini-2.5-pro-exp-03-25",
        ),
        (
            "config suffix takes priority",
            vec![(
                "claude",
                vec![alias(
                    "claude-sonnet-4-5-20250514(low)",
                    "claude-sonnet-4-5",
                )],
            )],
            "claude",
            "claude-sonnet-4-5(high)",
            "claude-sonnet-4-5-20250514(low)",
        ),
        (
            "auto suffix preserved",
            vec![("antigravity", vec![gemini()])],
            "antigravity",
            "gemini-2.5-pro(auto)",
            "gemini-2.5-pro-exp-03-25(auto)",
        ),
        (
            "none suffix preserved",
            vec![("antigravity", vec![gemini()])],
            "antigravity",
            "gemini-2.5-pro(none)",
            "gemini-2.5-pro-exp-03-25(none)",
        ),
        (
            "kimi suffix preserved",
            vec![("kimi", vec![alias("kimi-k2.5", "k2.5")])],
            "kimi",
            "k2.5(high)",
            "kimi-k2.5(high)",
        ),
        (
            "meta suffix preserved",
            vec![("meta", vec![alias("muse-spark-1.3", "muse-latest")])],
            "meta",
            "muse-latest(high)",
            "muse-spark-1.3(high)",
        ),
        (
            "case insensitive alias lookup with suffix",
            vec![(
                "antigravity",
                vec![alias("gemini-2.5-pro-exp-03-25", "Gemini-2.5-Pro")],
            )],
            "antigravity",
            "gemini-2.5-pro(high)",
            "gemini-2.5-pro-exp-03-25(high)",
        ),
        (
            "no alias returns empty",
            vec![("antigravity", vec![gemini()])],
            "antigravity",
            "unknown-model(high)",
            "",
        ),
        (
            "wrong channel returns empty",
            vec![("antigravity", vec![gemini()])],
            "claude",
            "gemini-2.5-pro(high)",
            "",
        ),
        (
            "empty suffix filtered out",
            vec![("antigravity", vec![gemini()])],
            "antigravity",
            "gemini-2.5-pro()",
            "gemini-2.5-pro-exp-03-25",
        ),
        (
            "incomplete suffix treated as no suffix",
            vec![(
                "antigravity",
                vec![alias("gemini-2.5-pro-exp-03-25", "gemini-2.5-pro(high")],
            )],
            "antigravity",
            "gemini-2.5-pro(high",
            "gemini-2.5-pro-exp-03-25",
        ),
    ];
    for (name, aliases, channel, input, want) in cases {
        let h = manager_with(aliases);
        let auth = create_auth_for_channel(channel);
        let got = with_resolver(&h, |r| {
            r.resolve_oauth_model_alias_with_result(&auth, input)
                .upstream_model
        });
        assert_eq!(got, want, "{name}: resolveOAuthUpstreamModel({input:?})");
    }
}

#[test]
fn oauth_model_alias_channel_api_key_only_provider_unsupported() {
    assert_eq!(oauth_model_alias_channel("gemini", "oauth"), "");
}

#[test]
fn oauth_model_alias_channel_kimi() {
    for provider in ["kimi", "kimi-ai", "kimi.ai", "kimi.com"] {
        assert_eq!(
            oauth_model_alias_channel(provider, "oauth"),
            provider,
            "OAuthModelAliasChannel({provider:?})"
        );
    }
}

#[test]
fn oauth_model_alias_channel_meta() {
    assert_eq!(oauth_model_alias_channel("meta", "oauth"), "meta");
    assert_eq!(oauth_model_alias_channel("meta", "api_key"), "");
}

#[test]
fn oauth_model_alias_channel_plugin_provider() {
    assert_eq!(
        oauth_model_alias_channel(" Sample-Provider ", "oauth"),
        "sample-provider"
    );
    assert_eq!(oauth_model_alias_channel("sample-provider", "api_key"), "");
}

#[test]
fn apply_oauth_model_alias_suffix_preservation() {
    let h = manager_with(vec![(
        "antigravity",
        vec![alias("gemini-2.5-pro-exp-03-25", "gemini-2.5-pro")],
    )]);
    let auth = auth("test-auth-id", "antigravity");
    let got = with_resolver(&h, |r| {
        r.apply_oauth_model_alias(&auth, "gemini-2.5-pro(8192)")
    });
    assert_eq!(got, "gemini-2.5-pro-exp-03-25(8192)");
}

#[test]
fn apply_oauth_model_alias_force_mapping_same_base_preserves_suffix() {
    let h = manager_with(vec![(
        "antigravity",
        vec![forced("gemini-2.5-pro", "gemini-2.5-pro(8192)")],
    )]);
    let auth = auth("test-auth-id", "antigravity");
    let got = with_resolver(&h, |r| {
        r.apply_oauth_model_alias(&auth, "gemini-2.5-pro(8192)")
    });
    assert_eq!(got, "gemini-2.5-pro(8192)");
}

#[test]
fn apply_oauth_model_alias_per_auth_force_mapping_same_base_preserves_suffix() {
    let h = Harness::new(Settings::default());
    let auth = with_attributes(
        "test-auth-id",
        "antigravity",
        &[(
            "model_aliases",
            r#"[{"name":"gemini-2.5-pro","alias":"gemini-2.5-pro(8192)","force-mapping":true}]"#,
        )],
    );
    let got = with_resolver(&h, |r| {
        r.apply_oauth_model_alias(&auth, "gemini-2.5-pro(8192)")
    });
    assert_eq!(got, "gemini-2.5-pro(8192)");
}

#[test]
fn apply_oauth_model_alias_per_auth_overrides_global_alias() {
    let h = manager_with(vec![("codex", vec![alias("gpt-5-global", "gpt-5.5")])]);
    let auth = with_attributes(
        "codex-auth-id",
        "codex",
        &[
            ("auth_kind", "oauth"),
            (
                "model_aliases",
                r#"[{"name":"gpt-5.3-codex-spark","alias":"gpt-5.5"}]"#,
            ),
        ],
    );
    let got = with_resolver(&h, |r| r.apply_oauth_model_alias(&auth, "gpt-5.5(high)"));
    assert_eq!(got, "gpt-5.3-codex-spark(high)");
}

#[test]
fn apply_oauth_model_alias_per_auth_alias_skips_api_key() {
    let h = Harness::new(Settings::default());
    let auth = with_attributes(
        "codex-api-key-auth",
        "codex",
        &[
            ("auth_kind", "api_key"),
            (
                "model_aliases",
                r#"[{"name":"gpt-5.3-codex-spark","alias":"gpt-5.5"}]"#,
            ),
        ],
    );
    let got = with_resolver(&h, |r| r.apply_oauth_model_alias(&auth, "gpt-5.5"));
    assert_eq!(got, "gpt-5.5");
}

#[test]
fn apply_oauth_model_alias_devin() {
    let h = manager_with(vec![(
        "devin",
        vec![forced("devin/claude-fable-5-1", "fable-5-1")],
    )]);
    let auth = with_attributes("devin-auth", "devin", &[("auth_kind", "oauth")]);
    with_resolver(&h, |r| {
        assert_eq!(
            r.apply_oauth_model_alias(&auth, "fable-5-1"),
            "devin/claude-fable-5-1"
        );
        // Suffix preservation with Devin thinking effort.
        assert_eq!(
            r.apply_oauth_model_alias(&auth, "fable-5-1(max)"),
            "devin/claude-fable-5-1(max)"
        );
        let result = r.apply_oauth_model_alias_with_result(&auth, "fable-5-1(max)");
        assert_eq!(result.upstream_model, "devin/claude-fable-5-1(max)");
        assert!(result.force_mapping);
        assert_eq!(result.original_alias, "fable-5-1");
    });
}

#[test]
fn apply_oauth_model_alias_meta() {
    let h = manager_with(vec![(
        "meta",
        vec![forced("muse-spark-1.3", "muse-latest")],
    )]);
    // Meta OAuth credentials mint an LLM API key; aliases must still apply.
    let auth = with_attributes(
        "meta-auth",
        "meta",
        &[
            ("auth_kind", "oauth"),
            ("api_key", "LLM|minted"),
            ("dca_token", "dca:token"),
        ],
    );
    with_resolver(&h, |r| {
        assert_eq!(
            r.apply_oauth_model_alias(&auth, "muse-latest"),
            "muse-spark-1.3"
        );
        assert_eq!(
            r.apply_oauth_model_alias(&auth, "muse-latest(max)"),
            "muse-spark-1.3(max)"
        );
        let result = r.apply_oauth_model_alias_with_result(&auth, "muse-latest(max)");
        assert_eq!(result.upstream_model, "muse-spark-1.3(max)");
        assert!(result.force_mapping);
        assert_eq!(result.original_alias, "muse-latest");
    });
}

#[test]
fn apply_oauth_model_alias_plugin_provider() {
    let h = manager_with(vec![(
        "sample-provider",
        vec![alias("sample-model-latest", "sample-latest")],
    )]);
    let auth = with_attributes(
        "sample-provider-auth",
        "sample-provider",
        &[("auth_kind", "oauth")],
    );
    let got = with_resolver(&h, |r| r.apply_oauth_model_alias(&auth, "sample-latest"));
    assert_eq!(got, "sample-model-latest");
}

#[test]
fn apply_oauth_model_alias_plugin_provider_skips_api_key() {
    let h = manager_with(vec![(
        "sample-provider",
        vec![alias("sample-model-latest", "sample-latest")],
    )]);
    let auth = with_attributes(
        "sample-provider-auth",
        "sample-provider",
        &[("auth_kind", "api_key")],
    );
    let got = with_resolver(&h, |r| r.apply_oauth_model_alias(&auth, "sample-latest"));
    assert_eq!(got, "sample-latest");
}

#[test]
fn apply_oauth_model_alias_with_result_force_mapping_uses_config_alias_not_request_suffix() {
    let h = manager_with(vec![("codex", vec![forced("gpt-5.4", "gpt-5.4-fast")])]);
    let auth = auth("t", "codex");
    let res = with_resolver(&h, |r| {
        r.apply_oauth_model_alias_with_result(&auth, "gpt-5.4-fast(high)")
    });
    assert_eq!(res.upstream_model, "gpt-5.4(high)");
    assert_eq!(res.original_alias, "gpt-5.4-fast");
}

#[test]
fn apply_oauth_model_alias_with_result_prefers_exact_suffixed_alias() {
    let h = manager_with(vec![(
        "codex",
        vec![
            alias("base-upstream", "public"),
            forced("low-upstream", "public(low)"),
        ],
    )]);
    let auth = auth("exact-suffix", "codex");
    let result = with_resolver(&h, |r| {
        r.apply_oauth_model_alias_with_result(&auth, "public(low)")
    });
    assert_eq!(result.upstream_model, "low-upstream(low)", "{result:?}");
    assert!(result.force_mapping, "{result:?}");
}

#[test]
fn apply_oauth_model_alias_with_result_no_force_mapping_preserves_requested_model_in_original_alias()
 {
    let h = manager_with(vec![("codex", vec![alias("gpt-5.4", "gpt-5.4-fast")])]);
    let auth = auth("t", "codex");
    let res = with_resolver(&h, |r| {
        r.apply_oauth_model_alias_with_result(&auth, "gpt-5.4-fast(high)")
    });
    assert!(!res.force_mapping);
    assert_eq!(res.original_alias, "gpt-5.4-fast(high)");
}
