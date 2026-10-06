// Ported from CLIProxyAPI internal/config/config_normalization.go
// (normalizeModelPrefix, NormalizeHeaders, NormalizeExcludedModels,
// NormalizeOAuthExcludedModels, SanitizeGeminiKeys,
// SanitizeInteractionsKeys, sanitizeGeminiKeyEntries,
// formatGeminiKeyDedupID, SanitizeCodexKeys, SanitizeXAIKeys,
// SanitizeMetaKeys, sanitizeMetaKeyEntries, SanitizeClaudeKeys,
// SanitizeOpenAICompatibility, SanitizeOAuthModelAlias,
// SanitizeOAuthRequestScopedErrors), vertex_compat.go
// (SanitizeVertexCompatKeys), internal/api/handlers/management/
// config_lists.go (normalizeOpenAICompatibilityEntry, normalizeClaudeKey,
// normalizeCodexKey, normalizeVertexCompatKey, sanitizedOAuthModelAlias,
// sanitizedOAuthRequestScopedErrors) and config_basic.go
// (normalizeRoutingStrategy) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The clean-ups a management write applies to what it changes, as
//! upstream's handlers apply them before saving.
//!
//! The config's own clean-ups, which loading applies, are private to
//! `open-ferry-core`; the ones the handlers call are repeated here, and
//! the tests check they agree with loading.
//!
//! Deviations from upstream:
//! - Maps iterate in key order rather than Go's random order: where two
//!   channels lower-case to the same name, the one that sorts last wins.
//! - The clean-ups of settings this port ignores (Claude's `cloak` and
//!   `fingerprint-profile`, Codex's `disable-codex-cloaking`) are left out.

use std::collections::{BTreeMap, HashSet};

use open_ferry_core::auth::synthesizer::format_sorted_headers;
use open_ferry_core::config::{
    ClaudeKey, CodexKey, GeminiKey, OAuthModelAlias, OpenAiCompatibility, RequestScopedErrorRule,
    VertexCompatKey,
};
use open_ferry_translate::go::{equal_fold, to_lower};

/// The Meta base URL a key without one gets.
pub(crate) const META_BASE_URL: &str = "https://api.meta.ai/v1";

/// Upstream's `normalizeModelPrefix`: trimmed of space and slashes; empty
/// when it still contains a slash.
pub(crate) fn normalize_model_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim().trim_matches('/');
    if trimmed.contains('/') {
        String::new()
    } else {
        trimmed.to_owned()
    }
}

/// Upstream's `NormalizeHeaders`: names and values trimmed, empty pairs
/// dropped.
pub(crate) fn normalize_headers(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(key, value)| (key.trim(), value.trim()))
        .filter(|(key, value)| !key.is_empty() && !value.is_empty())
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

/// Upstream's `NormalizeExcludedModels`: trimmed, lower case, without
/// empties or repeats, in first-seen order.
pub(crate) fn normalize_excluded_models(models: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    models
        .iter()
        .map(|model| to_lower(model.trim()))
        .filter(|model| !model.is_empty() && seen.insert(model.clone()))
        .collect()
}

/// Upstream's `NormalizeOAuthExcludedModels`: channels trimmed and in lower
/// case, their models normalized, and channels left empty dropped.
pub(crate) fn normalize_oauth_excluded_models(
    entries: &BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for (provider, models) in entries {
        let key = to_lower(provider.trim());
        let models = normalize_excluded_models(models);
        if !key.is_empty() && !models.is_empty() {
            out.insert(key, models);
        }
    }
    out
}

/// Upstream's `SanitizeGeminiKeys` and `SanitizeInteractionsKeys`: drops
/// entries with neither a key nor a base URL, cleans up the rest, and keeps
/// the first of entries alike in key, base URL, proxy, prefix and headers.
pub(crate) fn sanitize_gemini_keys(keys: &mut Vec<GeminiKey>) {
    let mut seen = HashSet::new();
    keys.retain_mut(|key| {
        key.api_key = key.api_key.trim().to_owned();
        key.base_url = key.base_url.trim().to_owned();
        if key.api_key.is_empty() && key.base_url.is_empty() {
            return false;
        }
        key.prefix = normalize_model_prefix(&key.prefix);
        key.proxy_url = key.proxy_url.trim().to_owned();
        key.headers = normalize_headers(&key.headers);
        key.excluded_models = normalize_excluded_models(&key.excluded_models);
        seen.insert(
            [
                key.api_key.as_str(),
                &key.base_url,
                &key.proxy_url,
                &key.prefix,
                &format_sorted_headers(&key.headers),
            ]
            .join("\0"),
        )
    });
}

/// Upstream's `SanitizeVertexCompatKeys`: drops entries without a key and
/// models without both a name and an alias, cleans up the rest, and keeps
/// the first of entries alike in key and base URL.
pub(crate) fn sanitize_vertex_keys(keys: &mut Vec<VertexCompatKey>) {
    let mut seen = HashSet::new();
    keys.retain_mut(|key| {
        key.api_key = key.api_key.trim().to_owned();
        if key.api_key.is_empty() {
            return false;
        }
        key.prefix = normalize_model_prefix(&key.prefix);
        key.base_url = key.base_url.trim().to_owned();
        key.proxy_url = key.proxy_url.trim().to_owned();
        key.headers = normalize_headers(&key.headers);
        key.excluded_models = normalize_excluded_models(&key.excluded_models);
        key.models.retain_mut(|model| {
            model.alias = model.alias.trim().to_owned();
            model.name = model.name.trim().to_owned();
            !model.alias.is_empty() && !model.name.is_empty()
        });
        seen.insert(format!("{}|{}", key.api_key, key.base_url))
    });
}

/// Upstream's `SanitizeCodexKeys`: cleans up and drops keys without a base
/// URL.
pub(crate) fn sanitize_codex_keys(keys: &mut Vec<CodexKey>) {
    for key in keys.iter_mut() {
        key.prefix = normalize_model_prefix(&key.prefix);
        key.base_url = key.base_url.trim().to_owned();
        key.headers = normalize_headers(&key.headers);
        key.excluded_models = normalize_excluded_models(&key.excluded_models);
    }
    keys.retain(|key| !key.base_url.is_empty());
}

/// Upstream's `SanitizeXAIKeys`: the Codex keys' clean-up, and no alpha
/// search, which only Codex offers.
pub(crate) fn sanitize_xai_keys(keys: &mut Vec<CodexKey>) {
    sanitize_codex_keys(keys);
    for key in keys.iter_mut() {
        key.alpha_search = false;
    }
}

/// Upstream's `SanitizeMetaKeys`: drops entries without a key or with a
/// `dca:` token, defaults the base URL and cleans up the rest.
pub(crate) fn sanitize_meta_keys(keys: &mut Vec<CodexKey>) {
    keys.retain_mut(|key| {
        key.api_key = key.api_key.trim().to_owned();
        if key.api_key.is_empty() || key.api_key.starts_with("dca:") {
            return false;
        }
        key.prefix = normalize_model_prefix(&key.prefix);
        key.base_url = key.base_url.trim().to_owned();
        if key.base_url.is_empty() {
            key.base_url = META_BASE_URL.to_owned();
        }
        key.headers = normalize_headers(&key.headers);
        key.excluded_models = normalize_excluded_models(&key.excluded_models);
        key.alpha_search = false;
        true
    });
}

/// Upstream's `SanitizeClaudeKeys`.
pub(crate) fn sanitize_claude_keys(keys: &mut [ClaudeKey]) {
    for key in keys {
        key.prefix = normalize_model_prefix(&key.prefix);
        key.headers = normalize_headers(&key.headers);
        key.excluded_models = normalize_excluded_models(&key.excluded_models);
    }
}

/// Upstream's `SanitizeOpenAICompatibility`: names, prefixes, base URLs and
/// headers cleaned up, and providers without a base URL dropped.
pub(crate) fn sanitize_openai_compatibility(providers: &mut Vec<OpenAiCompatibility>) {
    for provider in providers.iter_mut() {
        provider.name = provider.name.trim().to_owned();
        provider.prefix = normalize_model_prefix(&provider.prefix);
        provider.base_url = provider.base_url.trim().to_owned();
        provider.headers = normalize_headers(&provider.headers);
    }
    providers.retain(|provider| !provider.base_url.is_empty());
}

/// Upstream's `SanitizeOAuthModelAlias`: trimmed, channels in lower case,
/// no empty or self aliases, each alias once per channel.
fn sanitize_oauth_model_alias(
    channels: &BTreeMap<String, Vec<OAuthModelAlias>>,
) -> BTreeMap<String, Vec<OAuthModelAlias>> {
    let mut out = BTreeMap::new();
    for (raw_channel, aliases) in channels {
        let channel = to_lower(raw_channel.trim());
        if channel.is_empty() || aliases.is_empty() {
            continue;
        }
        let mut seen = HashSet::new();
        let mut clean = Vec::new();
        for entry in aliases {
            let name = entry.name.trim();
            let alias = entry.alias.trim();
            if name.is_empty() || alias.is_empty() || equal_fold(name, alias) {
                continue;
            }
            if !seen.insert(to_lower(alias)) {
                continue;
            }
            clean.push(OAuthModelAlias {
                name: name.to_owned(),
                alias: alias.to_owned(),
                fork: entry.fork,
                display_name: entry.display_name.trim().to_owned(),
                force_mapping: entry.force_mapping,
            });
        }
        if !clean.is_empty() {
            out.insert(channel, clean);
        }
    }
    out
}

/// Upstream's `SanitizeOAuthRequestScopedErrors`: trimmed, actions in lower
/// case, and rules without a status, a match or an action dropped.
fn sanitize_oauth_request_scoped_errors(
    channels: &BTreeMap<String, Vec<RequestScopedErrorRule>>,
) -> BTreeMap<String, Vec<RequestScopedErrorRule>> {
    let mut out = BTreeMap::new();
    for (raw_channel, rules) in channels {
        let channel = to_lower(raw_channel.trim());
        if channel.is_empty() || rules.is_empty() {
            continue;
        }
        let trimmed = |items: &[String]| -> Vec<String> {
            items
                .iter()
                .map(|item| item.trim())
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let clean: Vec<RequestScopedErrorRule> = rules
            .iter()
            .map(|rule| RequestScopedErrorRule {
                status: rule.status,
                matches: trimmed(&rule.matches),
                match_regexr: trimmed(&rule.match_regexr),
                action: to_lower(rule.action.trim()),
            })
            .filter(|rule| {
                rule.status > 0
                    && !(rule.matches.is_empty() && rule.match_regexr.is_empty())
                    && !rule.action.is_empty()
            })
            .collect();
        if !clean.is_empty() {
            out.insert(channel, clean);
        }
    }
    out
}

/// Upstream's `sanitizedOAuthModelAlias`, which the handlers store: the
/// aliases cleaned up.
pub(crate) fn sanitized_oauth_model_alias(
    entries: &BTreeMap<String, Vec<OAuthModelAlias>>,
) -> BTreeMap<String, Vec<OAuthModelAlias>> {
    sanitize_oauth_model_alias(entries)
}

/// Upstream's `sanitizedOAuthRequestScopedErrors`, which the handlers
/// store: the rules cleaned up.
pub(crate) fn sanitized_oauth_request_scoped_errors(
    entries: &BTreeMap<String, Vec<RequestScopedErrorRule>>,
) -> BTreeMap<String, Vec<RequestScopedErrorRule>> {
    sanitize_oauth_request_scoped_errors(entries)
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
/// `None` for a name it doesn't know.
pub(crate) fn normalize_routing_strategy(strategy: &str) -> Option<&'static str> {
    match to_lower(strategy.trim()).as_str() {
        "" | "round-robin" | "roundrobin" | "rr" => Some("round-robin"),
        "weighted-round-robin" | "weightedroundrobin" | "wrr" => Some("weighted-round-robin"),
        "fill-first" | "fillfirst" | "ff" => Some("fill-first"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use open_ferry_core::config::{
        ClaudeModel, CodexModel, Config, OpenAiCompatibilityApiKey, VertexCompatModel,
    };

    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|&item| item.to_owned()).collect()
    }

    /// What loading makes of `yaml`.
    fn loaded(yaml: &str) -> Config {
        Config::load_bytes(yaml.as_bytes()).unwrap()
    }

    // Not upstream's: the clean-ups repeated here agree with loading's.
    #[test]
    fn sanitizers_agree_with_loading() {
        let yaml = "\
gemini-api-key:
  - api-key: ' g '
    prefix: ' /p/ '
    headers: {' A ': ' 1 ', B: ' '}
    excluded-models: [' X ', x, '']
  - api-key: g
    prefix: p
    headers: {A: '1'}
  - base-url: ' https://b '
  - proxy-url: only
vertex-api-key:
  - api-key: ' v '
    models: [{name: ' n ', alias: ' a '}, {name: n}]
  - api-key: v
  - api-key: ''
codex-api-key:
  - api-key: c
    base-url: ' https://c '
    prefix: a/b
  - api-key: d
xai-api-key:
  - api-key: x
    base-url: https://x
    alpha-search: true
meta-api-key:
  - api-key: ' m '
  - api-key: 'dca:t'
claude-api-key:
  - api-key: c
    prefix: /q/
    excluded-models: [A]
openai-compatibility:
  - name: ' o '
    base-url: ' https://o '
    headers: {K: ' v '}
  - name: none
oauth-excluded-models:
  ' Codex ': [' A ', a]
  empty: []
oauth-model-alias:
  ' Claude ':
    - {name: ' m ', alias: ' n ', display-name: ' d '}
    - {name: m, alias: N}
    - {name: same, alias: SAME}
oauth-request-scoped-errors:
  Codex:
    - {status: 429, match: [' quota '], action: ' Stop '}
    - {status: 0, match: [x], action: stop}
";
        let want = loaded(yaml);
        let mut raw = loaded("");
        raw.gemini_api_key = vec![
            GeminiKey {
                api_key: " g ".into(),
                prefix: " /p/ ".into(),
                headers: BTreeMap::from([(" A ".into(), " 1 ".into()), ("B".into(), " ".into())]),
                excluded_models: strings(&[" X ", "x", ""]),
                ..GeminiKey::default()
            },
            GeminiKey {
                api_key: "g".into(),
                prefix: "p".into(),
                headers: BTreeMap::from([("A".into(), "1".into())]),
                ..GeminiKey::default()
            },
            GeminiKey {
                base_url: " https://b ".into(),
                ..GeminiKey::default()
            },
            GeminiKey {
                proxy_url: "only".into(),
                ..GeminiKey::default()
            },
        ];
        raw.vertex_api_key = vec![
            VertexCompatKey {
                api_key: " v ".into(),
                models: vec![
                    VertexCompatModel {
                        name: " n ".into(),
                        alias: " a ".into(),
                        ..VertexCompatModel::default()
                    },
                    VertexCompatModel {
                        name: "n".into(),
                        ..VertexCompatModel::default()
                    },
                ],
                ..VertexCompatKey::default()
            },
            VertexCompatKey {
                api_key: "v".into(),
                ..VertexCompatKey::default()
            },
            VertexCompatKey::default(),
        ];
        raw.codex_api_key = vec![
            CodexKey {
                api_key: "c".into(),
                base_url: " https://c ".into(),
                prefix: "a/b".into(),
                ..CodexKey::default()
            },
            CodexKey {
                api_key: "d".into(),
                ..CodexKey::default()
            },
        ];
        raw.xai_api_key = vec![CodexKey {
            api_key: "x".into(),
            base_url: "https://x".into(),
            alpha_search: true,
            ..CodexKey::default()
        }];
        raw.meta_api_key = vec![
            CodexKey {
                api_key: " m ".into(),
                ..CodexKey::default()
            },
            CodexKey {
                api_key: "dca:t".into(),
                ..CodexKey::default()
            },
        ];
        raw.claude_api_key = vec![ClaudeKey {
            api_key: "c".into(),
            prefix: "/q/".into(),
            excluded_models: strings(&["A"]),
            ..ClaudeKey::default()
        }];
        raw.openai_compatibility = vec![
            OpenAiCompatibility {
                name: " o ".into(),
                base_url: " https://o ".into(),
                headers: BTreeMap::from([("K".into(), " v ".into())]),
                ..OpenAiCompatibility::default()
            },
            OpenAiCompatibility {
                name: "none".into(),
                ..OpenAiCompatibility::default()
            },
        ];
        let mut config = raw.clone();
        sanitize_gemini_keys(&mut config.gemini_api_key);
        sanitize_vertex_keys(&mut config.vertex_api_key);
        sanitize_codex_keys(&mut config.codex_api_key);
        sanitize_xai_keys(&mut config.xai_api_key);
        sanitize_meta_keys(&mut config.meta_api_key);
        sanitize_claude_keys(&mut config.claude_api_key);
        sanitize_openai_compatibility(&mut config.openai_compatibility);
        config.oauth_excluded_models = normalize_oauth_excluded_models(&BTreeMap::from([
            (" Codex ".into(), strings(&[" A ", "a"])),
            ("empty".into(), Vec::new()),
        ]));
        config.oauth_model_alias = sanitized_oauth_model_alias(&BTreeMap::from([(
            " Claude ".into(),
            vec![
                OAuthModelAlias {
                    name: " m ".into(),
                    alias: " n ".into(),
                    display_name: " d ".into(),
                    ..OAuthModelAlias::default()
                },
                OAuthModelAlias {
                    name: "m".into(),
                    alias: "N".into(),
                    ..OAuthModelAlias::default()
                },
                OAuthModelAlias {
                    name: "same".into(),
                    alias: "SAME".into(),
                    ..OAuthModelAlias::default()
                },
            ],
        )]));
        config.oauth_request_scoped_errors =
            sanitized_oauth_request_scoped_errors(&BTreeMap::from([(
                "Codex".into(),
                vec![
                    RequestScopedErrorRule {
                        status: 429,
                        matches: strings(&[" quota "]),
                        action: " Stop ".into(),
                        ..RequestScopedErrorRule::default()
                    },
                    RequestScopedErrorRule {
                        status: 0,
                        matches: strings(&["x"]),
                        action: "stop".into(),
                        ..RequestScopedErrorRule::default()
                    },
                ],
            )]));
        assert!(config == want, "{config:#?}\n!=\n{want:#?}");
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
            ("random", None),
        ] {
            assert_eq!(normalize_routing_strategy(raw), want, "{raw}");
        }
    }
}
