// Ported from CLIProxyAPI internal/api/handlers/management/config_lists.go
// (normalizeOpenAICompatibilityEntry, normalizeClaudeKey, normalizeCodexKey,
// normalizeVertexCompatKey, sanitizedOAuthModelAlias,
// sanitizedOAuthRequestScopedErrors), config_apikey_disable.go
// (setConfigAPIKeyExcludedAll, toggleConfigAPIKeyExcludedAll) and
// config_basic.go (normalizeRoutingStrategy), and the tests from
// config_apikey_disable_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The clean-ups a management write applies to what it changes, as
//! upstream's handlers apply them before saving.
//!
//! The config's own clean-ups, the ones loading applies, are core's
//! ([`open_ferry_core::config::sanitize`]); they are re-exported here for
//! the handlers, next to the handlers' own normalizations.
//!
//! Deviations from upstream:
//! - Maps iterate in key order rather than Go's random order: where two
//!   channels lower-case to the same name, the one that sorts last wins.
//! - The clean-ups of settings this port ignores (Claude's `cloak` and
//!   `fingerprint-profile`, Codex's `disable-codex-cloaking`) are left out.
//! - The routing strategy may be open-ferry's own `quota`, which upstream
//!   refuses as an invalid strategy.

use std::collections::BTreeMap;

use open_ferry_core::auth::synthesizer::{StableIdGenerator, format_sorted_headers};
use open_ferry_core::config::sanitize;
pub(crate) use open_ferry_core::config::sanitize::{
    META_BASE_URL, normalize_excluded_models, normalize_headers, normalize_oauth_excluded_models,
    sanitize_claude_keys, sanitize_codex_keys, sanitize_gemini_keys, sanitize_meta_keys,
    sanitize_openai_compatibility, sanitize_vertex_keys, sanitize_xai_keys,
};
use open_ferry_core::config::{
    ClaudeKey, CodexKey, Config, OAuthModelAlias, OpenAiCompatibility, RequestScopedErrorRule,
    VertexCompatKey,
};
use open_ferry_translate::go::to_lower;

/// The `excluded-models` entry that turns a config API key off.
pub(crate) const DISABLE_PATTERN: &str = "*";

/// Upstream's `sanitizedOAuthModelAlias`, which the handlers store: the
/// aliases cleaned up.
pub(crate) fn sanitized_oauth_model_alias(
    entries: &BTreeMap<String, Vec<OAuthModelAlias>>,
) -> BTreeMap<String, Vec<OAuthModelAlias>> {
    sanitize::sanitize_oauth_model_alias(entries)
}

/// Upstream's `sanitizedOAuthRequestScopedErrors`, which the handlers
/// store: the rules cleaned up.
pub(crate) fn sanitized_oauth_request_scoped_errors(
    entries: &BTreeMap<String, Vec<RequestScopedErrorRule>>,
) -> BTreeMap<String, Vec<RequestScopedErrorRule>> {
    sanitize::sanitize_oauth_request_scoped_errors(entries)
}

/// Upstream's `normalizeOpenAICompatibilityEntry`: the base URL and the
/// API keys trimmed, the headers cleaned up.
pub(crate) fn normalize_openai_entry(entry: &mut OpenAiCompatibility) {
    entry.base_url = entry.base_url.trim().to_owned();
    entry.headers = normalize_headers(&entry.headers);
    for key in &mut entry.api_key_entries {
        key.api_key = key.api_key.trim().to_owned();
    }
}

/// Upstream's `normalizeClaudeKey`: trimmed and cleaned up, and models
/// without a name or an alias dropped.
pub(crate) fn normalize_claude_key(entry: &mut ClaudeKey) {
    entry.api_key = entry.api_key.trim().to_owned();
    entry.base_url = entry.base_url.trim().to_owned();
    entry.proxy_url = entry.proxy_url.trim().to_owned();
    entry.headers = normalize_headers(&entry.headers);
    entry.excluded_models = normalize_excluded_models(&entry.excluded_models);
    entry.models.retain_mut(|model| {
        model.name = model.name.trim().to_owned();
        model.alias = model.alias.trim().to_owned();
        !(model.name.is_empty() && model.alias.is_empty())
    });
}

/// Upstream's `normalizeCodexKey`, for Codex, xAI and Meta keys: trimmed
/// and cleaned up, and models without a name or an alias dropped.
pub(crate) fn normalize_codex_key(entry: &mut CodexKey) {
    entry.api_key = entry.api_key.trim().to_owned();
    entry.prefix = entry.prefix.trim().to_owned();
    entry.base_url = entry.base_url.trim().to_owned();
    entry.proxy_url = entry.proxy_url.trim().to_owned();
    entry.headers = normalize_headers(&entry.headers);
    entry.excluded_models = normalize_excluded_models(&entry.excluded_models);
    entry.models.retain_mut(|model| {
        model.name = model.name.trim().to_owned();
        model.alias = model.alias.trim().to_owned();
        !(model.name.is_empty() && model.alias.is_empty())
    });
}

/// Upstream's `normalizeVertexCompatKey`: trimmed and cleaned up, and
/// models without both a name and an alias dropped.
pub(crate) fn normalize_vertex_key(entry: &mut VertexCompatKey) {
    entry.api_key = entry.api_key.trim().to_owned();
    entry.prefix = entry.prefix.trim().to_owned();
    entry.base_url = entry.base_url.trim().to_owned();
    entry.proxy_url = entry.proxy_url.trim().to_owned();
    entry.headers = normalize_headers(&entry.headers);
    entry.excluded_models = normalize_excluded_models(&entry.excluded_models);
    entry.models.retain_mut(|model| {
        model.name = model.name.trim().to_owned();
        model.alias = model.alias.trim().to_owned();
        !model.name.is_empty() && !model.alias.is_empty()
    });
}

/// Upstream's `normalizeRoutingStrategy`: a strategy's canonical name, or
/// `None` for a name it doesn't know. open-ferry's own `quota` is known
/// too; upstream refuses it.
pub(crate) fn normalize_routing_strategy(strategy: &str) -> Option<&'static str> {
    match to_lower(strategy.trim()).as_str() {
        "" | "round-robin" | "roundrobin" | "rr" => Some("round-robin"),
        "weighted-round-robin" | "weightedroundrobin" | "wrr" => Some("weighted-round-robin"),
        "fill-first" | "fillfirst" | "ff" => Some("fill-first"),
        "quota" => Some("quota"),
        _ => None,
    }
}

/// Upstream's `setConfigAPIKeyExcludedAll`: `models` with
/// [`DISABLE_PATTERN`] added when `disable`, else with it removed,
/// normalized.
fn set_excluded_all(models: &[String], disable: bool) -> Vec<String> {
    if disable {
        if models.iter().any(|model| model.trim() == DISABLE_PATTERN) {
            return normalize_excluded_models(models);
        }
        let mut models = models.to_vec();
        models.push(DISABLE_PATTERN.to_owned());
        return normalize_excluded_models(&models);
    }
    let kept: Vec<String> = models
        .iter()
        .filter(|model| model.trim() != DISABLE_PATTERN)
        .cloned()
        .collect();
    normalize_excluded_models(&kept)
}

/// Why a config API key couldn't be turned on or off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToggleError {
    /// The credential's ID is empty (upstream's `auth id is empty`).
    EmptyId,
    /// No key in the config has the credential's ID.
    NotFound,
}

/// Upstream's `toggleConfigAPIKeyExcludedAll`: turns off (or back on) the
/// config API key whose credential has `auth_id`, by adding
/// [`DISABLE_PATTERN`] to its `excluded-models` (or removing it). The IDs
/// are made as loading makes them, over the Gemini, Interactions, Claude,
/// Codex, xAI, Meta and Vertex keys in turn.
pub(crate) fn toggle_excluded_all(
    config: &mut Config,
    auth_id: &str,
    disable: bool,
) -> Result<(), ToggleError> {
    let auth_id = auth_id.trim();
    if auth_id.is_empty() {
        return Err(ToggleError::EmptyId);
    }
    let mut ids = StableIdGenerator::new();
    let mut is_target = |kind: &str,
                         key: &str,
                         base: &str,
                         proxy: &str,
                         prefix: &str,
                         headers: &BTreeMap<String, String>| {
        let (id, _) = ids.next(
            kind,
            &[
                key.trim(),
                base.trim(),
                proxy.trim(),
                prefix.trim(),
                &format_sorted_headers(headers),
            ],
        );
        id == auth_id
    };
    for (kind, keys) in [
        ("gemini:apikey", &mut config.gemini_api_key),
        (
            "gemini-interactions:apikey",
            &mut config.interactions_api_key,
        ),
    ] {
        for entry in keys.iter_mut() {
            if entry.api_key.trim().is_empty() && entry.base_url.trim().is_empty() {
                continue;
            }
            let (key, base, proxy, prefix) = (
                &entry.api_key,
                &entry.base_url,
                &entry.proxy_url,
                &entry.prefix,
            );
            if is_target(kind, key, base, proxy, prefix, &entry.headers) {
                entry.excluded_models = set_excluded_all(&entry.excluded_models, disable);
                return Ok(());
            }
        }
    }
    for entry in &mut config.claude_api_key {
        if entry.api_key.trim().is_empty() && entry.base_url.trim().is_empty() {
            continue;
        }
        let (key, base, proxy, prefix) = (
            &entry.api_key,
            &entry.base_url,
            &entry.proxy_url,
            &entry.prefix,
        );
        if is_target("claude:apikey", key, base, proxy, prefix, &entry.headers) {
            entry.excluded_models = set_excluded_all(&entry.excluded_models, disable);
            return Ok(());
        }
    }
    for (kind, keys) in [
        ("codex:apikey", &mut config.codex_api_key),
        ("xai:apikey", &mut config.xai_api_key),
        ("meta:apikey", &mut config.meta_api_key),
    ] {
        for entry in keys.iter_mut() {
            if entry.api_key.trim().is_empty() && entry.base_url.trim().is_empty() {
                continue;
            }
            let (key, base, proxy, prefix) = (
                &entry.api_key,
                &entry.base_url,
                &entry.proxy_url,
                &entry.prefix,
            );
            if is_target(kind, key, base, proxy, prefix, &entry.headers) {
                entry.excluded_models = set_excluded_all(&entry.excluded_models, disable);
                return Ok(());
            }
        }
    }
    for entry in &mut config.vertex_api_key {
        let (id, _) = ids.next(
            "vertex:apikey",
            &[
                entry.api_key.trim(),
                entry.base_url.trim(),
                entry.proxy_url.trim(),
            ],
        );
        if id == auth_id {
            entry.excluded_models = set_excluded_all(&entry.excluded_models, disable);
            return Ok(());
        }
    }
    Err(ToggleError::NotFound)
}

#[cfg(test)]
mod tests {
    use open_ferry_core::config::{
        ClaudeModel, CodexModel, GeminiKey, OpenAiCompatibilityApiKey, VertexCompatModel,
    };

    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|&item| item.to_owned()).collect()
    }

    // Not upstream's: the handlers' own normalizations.
    #[test]
    fn handler_normalizations() {
        let mut claude = ClaudeKey {
            api_key: " k ".into(),
            prefix: " p ".into(),
            models: vec![
                ClaudeModel {
                    name: " n ".into(),
                    ..ClaudeModel::default()
                },
                ClaudeModel {
                    name: " ".into(),
                    alias: " ".into(),
                    ..ClaudeModel::default()
                },
            ],
            ..ClaudeKey::default()
        };
        normalize_claude_key(&mut claude);
        assert_eq!(claude.api_key, "k");
        assert_eq!(claude.prefix, " p ");
        assert_eq!(claude.models.len(), 1);
        assert_eq!(claude.models[0].name, "n");

        let mut codex = CodexKey {
            prefix: " p ".into(),
            models: vec![CodexModel {
                alias: " a ".into(),
                ..CodexModel::default()
            }],
            ..CodexKey::default()
        };
        normalize_codex_key(&mut codex);
        assert_eq!(codex.prefix, "p");
        assert_eq!(codex.models[0].alias, "a");

        let mut vertex = VertexCompatKey {
            models: vec![VertexCompatModel {
                name: "n".into(),
                ..VertexCompatModel::default()
            }],
            ..VertexCompatKey::default()
        };
        normalize_vertex_key(&mut vertex);
        assert!(vertex.models.is_empty());

        let mut openai = OpenAiCompatibility {
            name: " n ".into(),
            base_url: " b ".into(),
            api_key_entries: vec![OpenAiCompatibilityApiKey {
                api_key: " k ".into(),
                ..OpenAiCompatibilityApiKey::default()
            }],
            ..OpenAiCompatibility::default()
        };
        normalize_openai_entry(&mut openai);
        assert_eq!(openai.name, " n ");
        assert_eq!(openai.base_url, "b");
        assert_eq!(openai.api_key_entries[0].api_key, "k");

        for (raw, want) in [
            ("", Some("round-robin")),
            (" RR ", Some("round-robin")),
            ("WeightedRoundRobin", Some("weighted-round-robin")),
            ("ff", Some("fill-first")),
            (" Quota ", Some("quota")),
            ("random", None),
        ] {
            assert_eq!(normalize_routing_strategy(raw), want, "{raw}");
        }
    }

    /// Ported from upstream's config_apikey_disable_test.go
    /// (TestSetConfigAPIKeyExcludedAll), and the pattern given twice, with
    /// space or in upper case.
    #[test]
    fn excluded_all_is_added_once_and_removed() {
        assert_eq!(
            set_excluded_all(&strings(&["gpt-5"]), true),
            strings(&["gpt-5", "*"])
        );
        assert_eq!(
            set_excluded_all(&strings(&["gpt-5", "*"]), false),
            strings(&["gpt-5"])
        );
        assert_eq!(
            set_excluded_all(&strings(&["A"]), true),
            strings(&["a", "*"])
        );
        assert_eq!(
            set_excluded_all(&strings(&[" * ", "b"]), true),
            strings(&["*", "b"])
        );
        assert_eq!(
            set_excluded_all(&strings(&[" * ", "B", "*"]), false),
            strings(&["b"])
        );
        assert!(set_excluded_all(&[], false).is_empty());
    }

    /// The ID loading gives the first key of `kind` with `parts`.
    fn id(kind: &str, parts: &[&str]) -> String {
        StableIdGenerator::new().next(kind, parts).0
    }

    /// Ported from upstream's config_apikey_disable_test.go
    /// (TestToggleConfigAPIKeyExcludedAll_XAI).
    #[test]
    fn toggle_config_api_key_excluded_all_xai() {
        let mut config = Config::default();
        config.xai_api_key = vec![CodexKey {
            api_key: "xai-test".into(),
            base_url: "https://api.x.ai/v1".into(),
            ..CodexKey::default()
        }];
        let auth_id = id(
            "xai:apikey",
            &["xai-test", "https://api.x.ai/v1", "", "", ""],
        );
        assert_eq!(toggle_excluded_all(&mut config, &auth_id, true), Ok(()));
        assert_eq!(config.xai_api_key[0].excluded_models, ["*"]);
    }

    /// Ported from upstream's config_apikey_disable_test.go
    /// (TestToggleConfigAPIKeyExcludedAll_Meta).
    #[test]
    fn toggle_config_api_key_excluded_all_meta() {
        let mut config = Config::default();
        config.meta_api_key = vec![CodexKey {
            api_key: "meta-test".into(),
            base_url: "https://api.meta.ai/v1".into(),
            ..CodexKey::default()
        }];
        let auth_id = id(
            "meta:apikey",
            &["meta-test", "https://api.meta.ai/v1", "", "", ""],
        );
        assert_eq!(toggle_excluded_all(&mut config, &auth_id, true), Ok(()));
        assert_eq!(config.meta_api_key[0].excluded_models, ["*"]);
    }

    /// Ported from upstream's config_apikey_disable_test.go
    /// (TestToggleConfigAPIKeyExcludedAll_Codex).
    #[test]
    fn toggle_config_api_key_excluded_all_codex() {
        let mut config = Config::default();
        config.codex_api_key = vec![CodexKey {
            api_key: "sk-test".into(),
            base_url: "https://example.com/v1".into(),
            ..CodexKey::default()
        }];
        let auth_id = id(
            "codex:apikey",
            &["sk-test", "https://example.com/v1", "", "", ""],
        );
        assert_eq!(toggle_excluded_all(&mut config, &auth_id, true), Ok(()));
        assert_eq!(config.codex_api_key[0].excluded_models, ["*"]);
        assert_eq!(toggle_excluded_all(&mut config, &auth_id, false), Ok(()));
        assert!(config.codex_api_key[0].excluded_models.is_empty());
    }

    /// Ported from upstream's config_apikey_disable_test.go
    /// (TestToggleConfigAPIKeyExcludedAll_Vertex_NoBaseURL).
    #[test]
    fn toggle_config_api_key_excluded_all_vertex_no_base_url() {
        let mut config = Config::default();
        config.vertex_api_key = vec![VertexCompatKey {
            api_key: "vertex-key-only".into(),
            ..VertexCompatKey::default()
        }];
        let auth_id = id("vertex:apikey", &["vertex-key-only", "", ""]);
        assert_eq!(toggle_excluded_all(&mut config, &auth_id, true), Ok(()));
        assert_eq!(config.vertex_api_key[0].excluded_models, ["*"]);
    }

    /// Ported from upstream's config_apikey_disable_test.go
    /// (TestToggleConfigAPIKeyExcludedAll_EmptyKeyWithBaseURL).
    #[test]
    fn toggle_config_api_key_excluded_all_empty_key_with_base_url() {
        let mut config = Config::default();
        config.claude_api_key = vec![ClaudeKey {
            base_url: "https://custom-claude.example.com".into(),
            ..ClaudeKey::default()
        }];
        config.gemini_api_key = vec![GeminiKey {
            api_key: "   ".into(),
            base_url: "https://custom-gemini.example.com".into(),
            ..GeminiKey::default()
        }];
        let claude_id = id(
            "claude:apikey",
            &["", "https://custom-claude.example.com", "", "", ""],
        );
        let gemini_id = id(
            "gemini:apikey",
            &["", "https://custom-gemini.example.com", "", "", ""],
        );
        assert_eq!(toggle_excluded_all(&mut config, &claude_id, true), Ok(()));
        assert_eq!(config.claude_api_key[0].excluded_models, ["*"]);
        assert_eq!(toggle_excluded_all(&mut config, &gemini_id, true), Ok(()));
        assert_eq!(config.gemini_api_key[0].excluded_models, ["*"]);
    }

    // Not upstream's: a later key alike in every part gets the next ID, as
    // loading gives it; an ID no key has, or an empty one, is an error.
    #[test]
    fn toggle_config_api_key_finds_repeated_keys_and_refuses_others() {
        let key = CodexKey {
            api_key: "k".into(),
            base_url: "https://c".into(),
            ..CodexKey::default()
        };
        let mut config = Config::default();
        config.codex_api_key = vec![key.clone(), key];
        let mut ids = StableIdGenerator::new();
        let parts = ["k", "https://c", "", "", ""];
        let first = ids.next("codex:apikey", &parts).0;
        let second = ids.next("codex:apikey", &parts).0;
        assert_ne!(first, second);
        assert_eq!(toggle_excluded_all(&mut config, &second, true), Ok(()));
        assert!(config.codex_api_key[0].excluded_models.is_empty());
        assert_eq!(config.codex_api_key[1].excluded_models, ["*"]);
        assert_eq!(
            toggle_excluded_all(&mut config, &format!(" {first} "), true),
            Ok(())
        );
        assert_eq!(config.codex_api_key[0].excluded_models, ["*"]);

        assert_eq!(
            toggle_excluded_all(&mut config, "codex:apikey:none", true),
            Err(ToggleError::NotFound)
        );
        assert_eq!(
            toggle_excluded_all(&mut config, " ", true),
            Err(ToggleError::EmptyId)
        );
    }
}
