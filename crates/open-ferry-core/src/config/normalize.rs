// Ported from CLIProxyAPI internal/config/config_normalization.go,
// trusted_proxies.go, weight.go (ValidateCredentialWeights), the post-decode
// steps of config_load.go and parse.go, and internal/util/util.go
// (ResolveAuthDir) (v8.0.10, MIT), and internal/config/oauth_scope.go
// (ForAPIKey) (v8.0.11, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The checks and clean-ups upstream applies after decoding.
//!
//! Deviations from upstream:
//! - Upstream iterates Go maps in random order; here they iterate in key
//!   order. Where two map keys normalize to the same key (`Codex` and
//!   `codex`, or header names that differ only in surrounding space), the
//!   one that sorts last wins rather than an arbitrary one.
//! - Lower-casing uses Rust's Unicode rules, which differ from Go's
//!   `strings.ToLower` for a few characters (such as U+0130 and the final
//!   sigma).
//! - The management key isn't hashed with bcrypt or written back; it stays
//!   as written.
//! - Steps for sections this port ignores (other providers, plugins, pprof,
//!   logs, Redis, credential concurrency and in-flight, live media relay)
//!   are left out.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::net::IpAddr;
use std::path::PathBuf;

use super::paths::{self, Os};
use super::types::{
    ClaudeKey, CodexKey, Config, DEFAULT_PANEL_GITHUB_REPOSITORY, OAuthModelAlias,
    OAuthModelSetting, RequestScopedErrorRule,
};
use super::v8::check_weight;
use super::yaml::go_quote;
use super::{ConfigError, ConfigErrorKind};
use crate::auth::equal_fold;

impl Config {
    /// The config to use for a request made with an API key rather than an
    /// OAuth credential: settings a v8 document placed under
    /// `oauth.providers` are reset. Borrows when there are none.
    pub fn for_api_key(&self) -> Cow<'_, Config> {
        if self.oauth_only_fields.is_empty() {
            return Cow::Borrowed(self);
        }
        let mut filtered = self.clone();
        for field in &self.oauth_only_fields {
            // The other OAuth-scoped settings belong to ignored sections.
            match field.as_str() {
                "quota-exceeded.antigravity-credits" => {
                    filtered.quota_exceeded.antigravity_credits = false;
                }
                "ws-auth" => filtered.ws_auth = false,
                "codex-header-defaults.beta-features" => {
                    filtered.codex_header_defaults.beta_features.clear();
                }
                _ => {}
            }
        }
        filtered.oauth_only_fields.clear();
        Cow::Owned(filtered)
    }

    /// The auth directory as upstream resolves it: the default when unset, a
    /// leading `~` replaced by the home directory, and cleaned as Go cleans
    /// paths.
    pub fn resolve_auth_dir(&self) -> Result<PathBuf, ConfigError> {
        paths::resolve_auth_dir(Os::HOST, &self.auth_dir, paths::user_home_dir)
            .map(PathBuf::from)
            .map_err(|message| ConfigError::new(ConfigErrorKind::NoHomeDir, message))
    }
}

/// Upstream's checks and clean-ups after decoding, in its order.
pub(crate) fn post_process(config: &mut Config) -> Result<(), ConfigError> {
    validate_trusted_proxies(&config.trusted_proxies)?;
    validate_weights(config)?;
    let management = &mut config.remote_management;
    management.panel_github_repository = management.panel_github_repository.trim().to_owned();
    if management.panel_github_repository.is_empty() {
        management.panel_github_repository = DEFAULT_PANEL_GITHUB_REPOSITORY.to_owned();
    }
    if config.max_retry_credentials < 0 {
        config.max_retry_credentials = 0;
    }
    sanitize_codex_keys(&mut config.codex_api_key);
    config.codex_header_defaults.beta_features =
        config.codex_header_defaults.beta_features.trim().to_owned();
    sanitize_claude_keys(&mut config.claude_api_key);
    config.oauth_excluded_models = normalize_oauth_excluded_models(&config.oauth_excluded_models);
    config.oauth_model_alias = sanitize_oauth_model_alias(&config.oauth_model_alias);
    config.oauth_settings = sanitize_oauth_settings(&config.oauth_settings);
    config.oauth_request_scoped_errors =
        sanitize_oauth_request_scoped_errors(&config.oauth_request_scoped_errors);
    Ok(())
}

/// Upstream's `validateTrustedProxies`: each entry is an IP or a CIDR.
fn validate_trusted_proxies(entries: &[String]) -> Result<(), ConfigError> {
    for entry in entries {
        let quoted = go_quote(entry);
        if entry.is_empty() || entry.trim() != entry {
            return Err(ConfigError::new(
                ConfigErrorKind::Invalid,
                format!("invalid trusted-proxies entry {quoted}: expected an IP address or CIDR"),
            ));
        }
        if entry.parse::<IpAddr>().is_ok() || is_cidr(entry) {
            continue;
        }
        return Err(ConfigError::new(
            ConfigErrorKind::Invalid,
            format!("invalid trusted-proxies entry {quoted}: invalid CIDR address: {entry}"),
        ));
    }
    Ok(())
}

/// Go's `net.ParseCIDR` acceptance: an address without a zone, `/`, and a
/// decimal prefix length within the address size.
fn is_cidr(text: &str) -> bool {
    let Some((address, bits)) = text.split_once('/') else {
        return false;
    };
    let Ok(address) = address.parse::<IpAddr>() else {
        return false;
    };
    if bits.is_empty() || !bits.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let size = if address.is_ipv4() { 32 } else { 128 };
    // Leading zeros are fine; a long run of them still fits after trimming.
    let trimmed = bits.trim_start_matches('0');
    trimmed.len() <= 3 && trimmed.parse::<u32>().unwrap_or(0) <= size
}

/// Upstream's `ValidateCredentialWeights` for the key lists this port reads.
fn validate_weights(config: &Config) -> Result<(), ConfigError> {
    // Upstream checks Claude keys before Codex keys.
    check_family_weights(
        "claude-api-key",
        config.claude_api_key.iter().map(|key| key.weight),
    )?;
    check_family_weights(
        "codex-api-key",
        config.codex_api_key.iter().map(|key| key.weight),
    )
}

fn check_family_weights(
    family: &str,
    weights: impl Iterator<Item = Option<i64>>,
) -> Result<(), ConfigError> {
    for (index, weight) in weights.enumerate() {
        if let Some(weight) = weight
            && let Err(message) = check_weight(weight)
        {
            return Err(ConfigError::new(
                ConfigErrorKind::Invalid,
                format!("{family}[{index}].weight: {message}"),
            ));
        }
    }
    Ok(())
}

/// Upstream's `normalizeModelPrefix`: trimmed of space and slashes; empty
/// when it still contains a slash.
fn normalize_model_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim().trim_matches('/');
    if trimmed.contains('/') {
        String::new()
    } else {
        trimmed.to_owned()
    }
}

/// Upstream's `NormalizeHeaders`: names and values trimmed, empty pairs
/// dropped.
fn normalize_headers(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(key, value)| (key.trim(), value.trim()))
        .filter(|(key, value)| !key.is_empty() && !value.is_empty())
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

/// Upstream's `NormalizeExcludedModels`: trimmed, lower case, without
/// empties or repeats, in first-seen order.
fn normalize_excluded_models(models: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    models
        .iter()
        .map(|model| model.trim().to_lowercase())
        .filter(|model| !model.is_empty() && seen.insert(model.clone()))
        .collect()
}

/// Upstream's `NormalizeOAuthExcludedModels`.
fn normalize_oauth_excluded_models(
    entries: &BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for (provider, models) in entries {
        let key = provider.trim().to_lowercase();
        let models = normalize_excluded_models(models);
        if !key.is_empty() && !models.is_empty() {
            out.insert(key, models);
        }
    }
    out
}

/// Upstream's `SanitizeCodexKeys`: drops keys without a base URL.
fn sanitize_codex_keys(keys: &mut Vec<CodexKey>) {
    for key in keys.iter_mut() {
        key.prefix = normalize_model_prefix(&key.prefix);
        key.base_url = key.base_url.trim().to_owned();
        key.headers = normalize_headers(&key.headers);
        key.excluded_models = normalize_excluded_models(&key.excluded_models);
    }
    keys.retain(|key| !key.base_url.is_empty());
}

/// Upstream's `SanitizeClaudeKeys`.
fn sanitize_claude_keys(keys: &mut [ClaudeKey]) {
    for key in keys {
        key.prefix = normalize_model_prefix(&key.prefix);
        key.headers = normalize_headers(&key.headers);
        key.excluded_models = normalize_excluded_models(&key.excluded_models);
    }
}

/// Upstream's `SanitizeOAuthModelAlias`: trimmed, channels in lower case,
/// no empty or self aliases, each alias once per channel.
fn sanitize_oauth_model_alias(
    channels: &BTreeMap<String, Vec<OAuthModelAlias>>,
) -> BTreeMap<String, Vec<OAuthModelAlias>> {
    let mut out = BTreeMap::new();
    for (raw_channel, aliases) in channels {
        let channel = raw_channel.trim().to_lowercase();
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
            if !seen.insert(alias.to_lowercase()) {
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

/// Upstream's `SanitizeOAuthSettings`: trimmed, channels in lower case,
/// nameless entries dropped, and of entries with the same name and alias
/// the last kept, in its place among the rest.
fn sanitize_oauth_settings(
    channels: &BTreeMap<String, Vec<OAuthModelSetting>>,
) -> BTreeMap<String, Vec<OAuthModelSetting>> {
    let mut out = BTreeMap::new();
    for (raw_channel, settings) in channels {
        let channel = raw_channel.trim().to_lowercase();
        if channel.is_empty() || settings.is_empty() {
            continue;
        }
        let mut seen = HashSet::new();
        let mut reversed = Vec::new();
        for entry in settings.iter().rev() {
            let name = entry.name.trim();
            if name.is_empty() {
                continue;
            }
            let alias = entry.alias.trim();
            if !seen.insert(format!("{}->{}", name.to_lowercase(), alias.to_lowercase())) {
                continue;
            }
            reversed.push(OAuthModelSetting {
                name: name.to_owned(),
                alias: alias.to_owned(),
                max_context_length: entry.max_context_length,
            });
        }
        if !reversed.is_empty() {
            reversed.reverse();
            out.insert(channel, reversed);
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
        let channel = raw_channel.trim().to_lowercase();
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
                action: rule.action.trim().to_lowercase(),
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// The OAuth-scoped settings [`Config::for_api_key`] resets.
    fn typed_oauth_scoped_fields() -> BTreeSet<&'static str> {
        BTreeSet::from([
            "quota-exceeded.antigravity-credits",
            "ws-auth",
            "codex-header-defaults.beta-features",
        ])
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn trusted_proxies_like_upstream() {
        let check = |entries: &[&str]| {
            validate_trusted_proxies(&strings(entries)).map_err(|error| error.to_string())
        };
        assert_eq!(
            check(&[
                "127.0.0.1",
                "::1",
                "10.0.0.0/8",
                "fd00::/8",
                "::ffff:1.2.3.4",
                "1.2.3.4/032"
            ]),
            Ok(())
        );
        assert_eq!(
            check(&[""]),
            Err("invalid trusted-proxies entry \"\": expected an IP address or CIDR".to_owned())
        );
        assert_eq!(
            check(&[" 10.0.0.1"]),
            Err(
                "invalid trusted-proxies entry \" 10.0.0.1\": expected an IP address or CIDR"
                    .to_owned()
            )
        );
        for bad in [
            "localhost",
            "10.0.0.0/33",
            "10.0.0.0/",
            "10.0.0.0/+8",
            "fe80::1%eth0/64",
            "::/129",
            "01.2.3.4",
            "10.0.0.0/8/8",
        ] {
            assert_eq!(
                check(&[bad]),
                Err(format!(
                    "invalid trusted-proxies entry \"{bad}\": invalid CIDR address: {bad}"
                )),
            );
        }
        assert!(is_cidr("::/0128"));
        assert!(!is_cidr("::/0000000000000000000000000129"));
    }

    #[test]
    fn prefixes_headers_and_models() {
        assert_eq!(normalize_model_prefix(" /team/ "), "team");
        assert_eq!(normalize_model_prefix("a/b"), "");
        assert_eq!(normalize_model_prefix("//"), "");
        let headers = BTreeMap::from([
            (" X-A ".to_owned(), " 1 ".to_owned()),
            ("X-B".to_owned(), " ".to_owned()),
            (" ".to_owned(), "v".to_owned()),
        ]);
        assert_eq!(
            normalize_headers(&headers),
            BTreeMap::from([("X-A".to_owned(), "1".to_owned())])
        );
        assert_eq!(
            normalize_excluded_models(&strings(&[" GPT-5 ", "gpt-5", "", "o3*"])),
            strings(&["gpt-5", "o3*"])
        );
        let oauth = BTreeMap::from([
            (" Codex ".to_owned(), strings(&["A"])),
            ("claude".to_owned(), strings(&[" "])),
            (" ".to_owned(), strings(&["b"])),
        ]);
        assert_eq!(
            normalize_oauth_excluded_models(&oauth),
            BTreeMap::from([("codex".to_owned(), strings(&["a"]))])
        );
    }

    #[test]
    fn codex_keys_without_base_url_are_dropped() {
        let mut keys = vec![
            CodexKey {
                api_key: "a".to_owned(),
                base_url: " ".to_owned(),
                ..CodexKey::default()
            },
            CodexKey {
                api_key: "b".to_owned(),
                base_url: " https://example.invalid ".to_owned(),
                prefix: "/p/".to_owned(),
                ..CodexKey::default()
            },
        ];
        sanitize_codex_keys(&mut keys);
        assert_eq!(keys.len(), 1);
        assert_eq!(
            keys.first().map(|key| key.base_url.as_str()),
            Some("https://example.invalid")
        );
        assert_eq!(keys.first().map(|key| key.prefix.as_str()), Some("p"));
    }

    #[test]
    fn weights_report_the_first_bad_key() {
        let mut config = Config {
            codex_api_key: vec![CodexKey {
                weight: Some(2_000_000),
                ..CodexKey::default()
            }],
            claude_api_key: vec![
                ClaudeKey {
                    weight: Some(1_000_000),
                    ..ClaudeKey::default()
                },
                ClaudeKey {
                    weight: Some(1_000_001),
                    ..ClaudeKey::default()
                },
            ],
            ..Config::default()
        };
        assert_eq!(
            validate_weights(&config).map_err(|error| error.to_string()),
            Err("claude-api-key[1].weight: weight must not exceed 1000000".to_owned())
        );
        config.claude_api_key.clear();
        assert_eq!(
            validate_weights(&config).map_err(|error| error.to_string()),
            Err("codex-api-key[0].weight: weight must not exceed 1000000".to_owned())
        );
    }

    #[test]
    fn for_api_key_resets_oauth_scoped_settings() {
        let mut config = Config::default();
        assert!(matches!(config.for_api_key(), Cow::Borrowed(_)));
        config.quota_exceeded.antigravity_credits = true;
        config.codex_header_defaults.beta_features = "x".to_owned();
        config.oauth_only_fields = typed_oauth_scoped_fields()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let scoped = config.for_api_key();
        assert!(!scoped.ws_auth);
        assert!(!scoped.quota_exceeded.antigravity_credits);
        assert!(scoped.codex_header_defaults.beta_features.is_empty());
        assert!(scoped.oauth_only_fields.is_empty());
        assert!(config.ws_auth);
    }

    // oauth_model_alias_test.go

    fn alias(name: &str, alias: &str, fork: bool) -> OAuthModelAlias {
        OAuthModelAlias {
            name: name.to_owned(),
            alias: alias.to_owned(),
            fork,
            ..OAuthModelAlias::default()
        }
    }

    #[test]
    fn sanitize_oauth_model_alias_preserves_optional_fields() {
        let channels = BTreeMap::from([(
            " CoDeX ".to_owned(),
            vec![
                OAuthModelAlias {
                    display_name: " GPT Five ".to_owned(),
                    force_mapping: true,
                    ..alias(" gpt-5 ", " g5 ", true)
                },
                alias("gpt-6", "g6", false),
            ],
        )]);
        let want = vec![
            OAuthModelAlias {
                display_name: "GPT Five".to_owned(),
                force_mapping: true,
                ..alias("gpt-5", "g5", true)
            },
            alias("gpt-6", "g6", false),
        ];
        assert_eq!(
            sanitize_oauth_model_alias(&channels),
            BTreeMap::from([("codex".to_owned(), want)])
        );
    }

    #[test]
    fn sanitize_oauth_model_alias_allows_multiple_aliases_for_the_same_name() {
        let name = "gemini-claude-opus-4-5-thinking";
        let aliases = vec![
            alias(name, "claude-opus-4-5-20251101", true),
            alias(name, "claude-opus-4-5-20251101-thinking", true),
            alias(name, "claude-opus-4-5", true),
        ];
        let channels = BTreeMap::from([("antigravity".to_owned(), aliases.clone())]);
        assert_eq!(
            sanitize_oauth_model_alias(&channels),
            BTreeMap::from([("antigravity".to_owned(), aliases)])
        );
    }

    // oauth_request_scoped_errors_test.go

    #[test]
    fn sanitize_oauth_request_scoped_errors_drops_incomplete_rules() {
        let rule = |status, matches: &[&str], match_regexr: &[&str], action: &str| {
            RequestScopedErrorRule {
                status,
                matches: strings(matches),
                match_regexr: strings(match_regexr),
                action: action.to_owned(),
            }
        };
        let channels = BTreeMap::from([
            (
                " Vertex ".to_owned(),
                vec![
                    rule(
                        400,
                        &["  context_length  ", ""],
                        &["  ^error.*  ", ""],
                        " STOP ",
                    ),
                    rule(0, &["foo"], &[], "stop"),
                    rule(400, &[], &[], ""),
                ],
            ),
            (" empty-channel ".to_owned(), Vec::new()),
        ]);
        assert_eq!(
            sanitize_oauth_request_scoped_errors(&channels),
            BTreeMap::from([(
                "vertex".to_owned(),
                vec![rule(400, &["context_length"], &["^error.*"], "stop")]
            )])
        );
    }

    // oauth_settings_test.go

    fn setting(name: &str, max_context_length: i64) -> OAuthModelSetting {
        OAuthModelSetting {
            name: name.to_owned(),
            max_context_length,
            ..Default::default()
        }
    }

    #[test]
    fn sanitize_oauth_settings_keeps_the_later_duplicate() {
        let channels = BTreeMap::from([
            (
                " CODEX ".to_owned(),
                vec![
                    setting("  gpt-6-sol  ", 524_288),
                    setting("gpt-6-sol", 999_999),
                    setting("   ", 12_345),
                    setting("deepseek-v4-flash", 1_048_576),
                ],
            ),
            ("  ".to_owned(), vec![setting("ignored", 100)]),
        ]);
        assert_eq!(
            sanitize_oauth_settings(&channels),
            BTreeMap::from([(
                "codex".to_owned(),
                vec![
                    setting("gpt-6-sol", 999_999),
                    setting("deepseek-v4-flash", 1_048_576)
                ]
            )])
        );
    }

    #[test]
    fn sanitize_oauth_settings_keeps_last_occurrence_order_with_interleaved_rules() {
        let raw = vec![
            setting("upstream", 524_288),
            setting("public", 600_000),
            setting("upstream", 1_048_576),
        ];
        let resolved = OAuthModelSetting::resolve(&raw, "public", "upstream", "");
        assert_eq!(resolved.map(|s| s.max_context_length), Some(1_048_576));

        let channels = BTreeMap::from([("codex".to_owned(), raw)]);
        let sanitized = sanitize_oauth_settings(&channels);
        let codex = sanitized.get("codex").expect("codex settings");
        assert_eq!(
            codex,
            &[setting("public", 600_000), setting("upstream", 1_048_576)]
        );
        let resolved = OAuthModelSetting::resolve(codex, "public", "upstream", "");
        assert_eq!(resolved.map(|s| s.max_context_length), Some(1_048_576));
    }

    #[test]
    fn typed_scoped_fields_are_upstream_paths() {
        let scoped: BTreeSet<&str> = super::super::v8::oauth_scoped_paths().collect();
        assert!(typed_oauth_scoped_fields().is_subset(&scoped));
    }
}
