// Ported from CLIProxyAPI internal/config/config_normalization.go,
// vertex_compat.go (SanitizeVertexCompatKeys), trusted_proxies.go, weight.go (ValidateCredentialWeights), the post-decode
// steps of config_load.go and parse.go, config_validation.go
// (SanitizePayloadRules), internal/util/util.go
// (ResolveAuthDir), and internal/config/oauth_scope.go
// (ForAPIKey) (v8.0.20, MIT).
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
//! - Steps for sections this port ignores (Antigravity, Devin, cloaking,
//!   plugins, pprof, credential concurrency and in-flight, live media
//!   relay) are left out.
//! - open-ferry's own `claude-cli` list is cleaned up and checked here too
//!   ([`sanitize_claude_cli`]): a bad entry fails the load, as a bad weight
//!   does.
//! - So is open-ferry's own `management.separate-address`
//!   ([`super::management_address`]): a bad address fails the load.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::net::IpAddr;
use std::path::PathBuf;

use open_ferry_translate::go::to_lower;

use super::duration::parse_go_duration;
use super::paths::{self, Os};
use super::payload::sanitize_payload_rules;
use super::types::{
    ClaudeCli, ClaudeCliSystemPrompt, ClaudeKey, CodexKey, Config, DEFAULT_PANEL_GITHUB_REPOSITORY,
    GeminiKey, OAuthModelAlias, OAuthModelSetting, OpenAiCompatibility, RequestScopedErrorRule,
    VertexCompatKey,
};
use super::v8::check_weight;
use super::yaml::go_quote;
use super::{ConfigError, ConfigErrorKind};
use crate::auth::equal_fold;
use crate::auth::synthesizer::format_sorted_headers;

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
    if config.logs_max_total_size_mb < 0 {
        config.logs_max_total_size_mb = 0;
    }
    if config.error_logs_max_files < 0 {
        config.error_logs_max_files = 10;
    }
    if config.redis_usage_queue_retention_seconds <= 0 {
        config.redis_usage_queue_retention_seconds = 60;
    } else if config.redis_usage_queue_retention_seconds > 3600 {
        tracing::warn!(
            value = config.redis_usage_queue_retention_seconds,
            "redis-usage-queue-retention-seconds too large; clamping to 3600"
        );
        config.redis_usage_queue_retention_seconds = 3600;
    }
    if config.max_retry_credentials < 0 {
        config.max_retry_credentials = 0;
    }
    sanitize_gemini_keys(&mut config.gemini_api_key);
    sanitize_gemini_keys(&mut config.interactions_api_key);
    sanitize_vertex_keys(&mut config.vertex_api_key);
    sanitize_codex_keys(&mut config.codex_api_key);
    sanitize_xai_keys(&mut config.xai_api_key);
    sanitize_meta_keys(&mut config.meta_api_key);
    config.codex_header_defaults.beta_features =
        config.codex_header_defaults.beta_features.trim().to_owned();
    sanitize_claude_keys(&mut config.claude_api_key);
    sanitize_claude_cli(&mut config.claude_cli);
    validate_claude_cli(&config.claude_cli)?;
    super::management_address::clean_up(config)?;
    sanitize_openai_compatibility(&mut config.openai_compatibility);
    config.oauth_excluded_models = normalize_oauth_excluded_models(&config.oauth_excluded_models);
    config.oauth_model_alias = sanitize_oauth_model_alias(&config.oauth_model_alias);
    config.oauth_settings = sanitize_oauth_settings(&config.oauth_settings);
    config.oauth_request_scoped_errors =
        sanitize_oauth_request_scoped_errors(&config.oauth_request_scoped_errors);
    sanitize_payload_rules(&mut config.payload);
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

/// Upstream's `ValidateCredentialWeights` for the key lists this port reads,
/// in upstream's order.
fn validate_weights(config: &Config) -> Result<(), ConfigError> {
    check_family_weights(
        "gemini-api-key",
        config.gemini_api_key.iter().map(|key| key.weight),
    )?;
    check_family_weights(
        "interactions-api-key",
        config.interactions_api_key.iter().map(|key| key.weight),
    )?;
    check_family_weights(
        "claude-api-key",
        config.claude_api_key.iter().map(|key| key.weight),
    )?;
    check_family_weights(
        "vertex-api-key",
        config.vertex_api_key.iter().map(|key| key.weight),
    )?;
    check_family_weights(
        "codex-api-key",
        config.codex_api_key.iter().map(|key| key.weight),
    )?;
    check_family_weights(
        "xai-api-key",
        config.xai_api_key.iter().map(|key| key.weight),
    )?;
    check_family_weights(
        "meta-api-key",
        config.meta_api_key.iter().map(|key| key.weight),
    )?;
    check_family_weights(
        "claude-cli",
        config.claude_cli.iter().map(|entry| entry.weight),
    )?;
    for (provider_index, compat) in config.openai_compatibility.iter().enumerate() {
        for (key_index, entry) in compat.api_key_entries.iter().enumerate() {
            if let Some(weight) = entry.weight
                && let Err(message) = check_weight(weight)
            {
                return Err(ConfigError::new(
                    ConfigErrorKind::Invalid,
                    format!(
                        "openai-compatibility[{provider_index}].api-key-entries[{key_index}].weight: {message}"
                    ),
                ));
            }
        }
    }
    Ok(())
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

/// The base URL a Meta key without one gets.
pub const META_BASE_URL: &str = "https://api.meta.ai/v1";

/// Upstream's `normalizeModelPrefix`: trimmed of space and slashes; empty
/// when it still contains a slash.
pub fn normalize_model_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim().trim_matches('/');
    if trimmed.contains('/') {
        String::new()
    } else {
        trimmed.to_owned()
    }
}

/// Upstream's `NormalizeHeaders`: names and values trimmed, empty pairs
/// dropped.
pub fn normalize_headers(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(key, value)| (key.trim(), value.trim()))
        .filter(|(key, value)| !key.is_empty() && !value.is_empty())
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

/// Upstream's `NormalizeExcludedModels`: trimmed, lower case, without
/// empties or repeats, in first-seen order.
pub fn normalize_excluded_models(models: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    models
        .iter()
        .map(|model| to_lower(model.trim()))
        .filter(|model| !model.is_empty() && seen.insert(model.clone()))
        .collect()
}

/// Upstream's `NormalizeOAuthExcludedModels`.
pub fn normalize_oauth_excluded_models(
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

/// Upstream's `SanitizeGeminiKeys` and `SanitizeInteractionsKeys`
/// (`sanitizeGeminiKeyEntries`): drops entries with neither a key nor a base
/// URL, cleans up the rest, and keeps the first of entries alike in key,
/// base URL, proxy, prefix and headers.
pub fn sanitize_gemini_keys(keys: &mut Vec<GeminiKey>) {
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
        seen.insert(gemini_key_dedup_id(key))
    });
}

/// Upstream's `formatGeminiKeyDedupID`.
fn gemini_key_dedup_id(key: &GeminiKey) -> String {
    [
        key.api_key.as_str(),
        &key.base_url,
        &key.proxy_url,
        &key.prefix,
        &format_sorted_headers(&key.headers),
    ]
    .join("\0")
}

/// Upstream's `SanitizeVertexCompatKeys`: drops entries without a key and
/// models without both a name and an alias, cleans up the rest, and keeps
/// the first of entries alike in key and base URL.
pub fn sanitize_vertex_keys(keys: &mut Vec<VertexCompatKey>) {
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

/// Upstream's `SanitizeCodexKeys`: drops keys without a base URL.
pub fn sanitize_codex_keys(keys: &mut Vec<CodexKey>) {
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
pub fn sanitize_xai_keys(keys: &mut Vec<CodexKey>) {
    sanitize_codex_keys(keys);
    for key in keys.iter_mut() {
        key.alpha_search = false;
    }
}

/// Upstream's `SanitizeMetaKeys` (`sanitizeMetaKeyEntries`): drops entries
/// without a key or with a `dca:` token, which needs an OAuth credential
/// file, defaults the base URL and cleans up the rest.
pub fn sanitize_meta_keys(keys: &mut Vec<CodexKey>) {
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
pub fn sanitize_claude_keys(keys: &mut [ClaudeKey]) {
    for key in keys {
        key.prefix = normalize_model_prefix(&key.prefix);
        key.headers = normalize_headers(&key.headers);
        key.excluded_models = normalize_excluded_models(&key.excluded_models);
    }
}

/// Cleans up the `claude-cli` entries (not upstream's): the name, command,
/// config directory and timeout trimmed, the system prompt mode in lower
/// case, and the prefix and excluded models as for the API keys.
pub fn sanitize_claude_cli(entries: &mut [ClaudeCli]) {
    for entry in entries {
        entry.name = entry.name.trim().to_owned();
        entry.command = entry.command.trim().to_owned();
        entry.config_dir = entry.config_dir.trim().to_owned();
        entry.system_prompt = to_lower(entry.system_prompt.trim());
        entry.timeout = entry.timeout.trim().to_owned();
        entry.prefix = normalize_model_prefix(&entry.prefix);
        entry.excluded_models = normalize_excluded_models(&entry.excluded_models);
    }
}

/// Checks the cleaned-up `claude-cli` entries (not upstream's): each has a
/// name that no other entry has (ignoring case), a known system prompt mode,
/// a `max-concurrency` that isn't negative and a `timeout` that is empty or
/// a positive Go duration. The error names the entry and the key, not the
/// value.
pub(crate) fn validate_claude_cli(entries: &[ClaudeCli]) -> Result<(), ConfigError> {
    let mut names = HashSet::new();
    for (index, entry) in entries.iter().enumerate() {
        let invalid = |message: &str| {
            ConfigError::new(
                ConfigErrorKind::Invalid,
                format!("claude-cli[{index}].{message}"),
            )
        };
        if entry.name.is_empty() {
            return Err(invalid("name: a name is required"));
        }
        if !names.insert(to_lower(&entry.name)) {
            return Err(invalid("name: another claude-cli entry has this name"));
        }
        if ClaudeCliSystemPrompt::parse(&entry.system_prompt).is_none() {
            return Err(invalid("system-prompt: must be replace or append"));
        }
        if entry.max_concurrency < 0 {
            return Err(invalid("max-concurrency: must not be negative"));
        }
        if !entry.timeout.is_empty()
            && !parse_go_duration(&entry.timeout).is_some_and(|nanos| nanos > 0)
        {
            return Err(invalid("timeout: must be a positive duration such as 10m"));
        }
    }
    Ok(())
}

/// Upstream's `SanitizeOpenAICompatibility`: names, prefixes, base URLs and
/// headers cleaned up, and providers without a base URL dropped.
pub fn sanitize_openai_compatibility(providers: &mut Vec<OpenAiCompatibility>) {
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
pub fn sanitize_oauth_model_alias(
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

/// Upstream's `SanitizeOAuthSettings`: trimmed, channels in lower case,
/// nameless entries dropped, and of entries with the same name and alias
/// the last kept, in its place among the rest.
fn sanitize_oauth_settings(
    channels: &BTreeMap<String, Vec<OAuthModelSetting>>,
) -> BTreeMap<String, Vec<OAuthModelSetting>> {
    let mut out = BTreeMap::new();
    for (raw_channel, settings) in channels {
        let channel = to_lower(raw_channel.trim());
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
            if !seen.insert(format!("{}->{}", to_lower(name), to_lower(alias))) {
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
pub fn sanitize_oauth_request_scoped_errors(
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::super::types::{OpenAiCompatibilityApiKey, VertexCompatModel};
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
        config.codex_api_key.clear();
        // Not upstream's: the other families, in upstream's order.
        let bad = |weight| GeminiKey {
            weight: Some(weight),
            ..GeminiKey::default()
        };
        let bad_codex = |weight| CodexKey {
            weight: Some(weight),
            ..CodexKey::default()
        };
        config.meta_api_key = vec![bad_codex(-1), bad_codex(1_000_001)];
        config.xai_api_key = vec![bad_codex(1_000_001)];
        config.interactions_api_key = vec![bad(1_000_001)];
        for want in [
            "interactions-api-key[0].weight",
            "xai-api-key[0].weight",
            "meta-api-key[1].weight",
        ] {
            assert_eq!(
                validate_weights(&config).map_err(|error| error.to_string()),
                Err(format!("{want}: weight must not exceed 1000000"))
            );
            if want.starts_with("interactions") {
                config.interactions_api_key.clear();
            } else {
                config.xai_api_key.clear();
            }
        }
        config.meta_api_key.clear();
        config.openai_compatibility = vec![
            OpenAiCompatibility {
                api_key_entries: vec![OpenAiCompatibilityApiKey {
                    weight: Some(0),
                    ..OpenAiCompatibilityApiKey::default()
                }],
                ..OpenAiCompatibility::default()
            },
            OpenAiCompatibility {
                api_key_entries: vec![
                    OpenAiCompatibilityApiKey::default(),
                    OpenAiCompatibilityApiKey {
                        weight: Some(2_000_000),
                        ..OpenAiCompatibilityApiKey::default()
                    },
                ],
                ..OpenAiCompatibility::default()
            },
        ];
        assert_eq!(
            validate_weights(&config).map_err(|error| error.to_string()),
            Err(
                "openai-compatibility[1].api-key-entries[1].weight: weight must not exceed 1000000"
                    .to_owned()
            )
        );
    }

    // gemini_keys_normalization_test.go:
    // TestSanitizeGeminiKeys_AllowsEmptyAPIKeyWithBaseURL
    #[test]
    fn sanitize_gemini_keys_allows_empty_api_key_with_base_url() {
        let base = "https://custom-gemini.example.com";
        let header =
            |name: &str, value: &str| BTreeMap::from([(name.to_owned(), value.to_owned())]);
        let mut keys = vec![
            GeminiKey::default(),
            GeminiKey {
                api_key: "  ".to_owned(),
                ..GeminiKey::default()
            },
            GeminiKey {
                base_url: base.to_owned(),
                headers: header("Header-A", "1"),
                ..GeminiKey::default()
            },
            GeminiKey {
                base_url: base.to_owned(),
                headers: header("Header-B", "2"),
                ..GeminiKey::default()
            },
            GeminiKey {
                api_key: "key-1".to_owned(),
                base_url: base.to_owned(),
                ..GeminiKey::default()
            },
        ];
        let interactions_base = "https://custom-interactions.example.com";
        let mut interactions = vec![
            GeminiKey::default(),
            GeminiKey {
                api_key: "  ".to_owned(),
                ..GeminiKey::default()
            },
            GeminiKey {
                base_url: interactions_base.to_owned(),
                ..GeminiKey::default()
            },
        ];
        sanitize_gemini_keys(&mut keys);
        sanitize_gemini_keys(&mut interactions);
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0].base_url, base);
        assert_eq!(interactions.len(), 1);
        assert_eq!(interactions[0].base_url, interactions_base);
    }

    // xai_alpha_search_test.go:
    // TestSanitizeXAIKeysClearsCodexAlphaSearchCapability
    #[test]
    fn sanitize_xai_keys_clears_codex_alpha_search_capability() {
        let mut keys = vec![CodexKey {
            api_key: "xai-key".to_owned(),
            base_url: "https://api.x.ai/v1".to_owned(),
            alpha_search: true,
            ..CodexKey::default()
        }];
        sanitize_xai_keys(&mut keys);
        assert_eq!(keys.len(), 1);
        assert!(!keys[0].alpha_search);
    }

    // Not upstream's: the rest of sanitizeMetaKeyEntries (config_meta_test.go
    // is ported in `load`).
    #[test]
    fn meta_keys_are_cleaned_up() {
        let mut keys = vec![CodexKey {
            api_key: " LLM|key ".to_owned(),
            prefix: " /team/ ".to_owned(),
            base_url: " https://meta.example.com/v1 ".to_owned(),
            headers: BTreeMap::from([(" X-A ".to_owned(), " 1 ".to_owned())]),
            excluded_models: strings(&[" Muse-1 ", "muse-1"]),
            alpha_search: true,
            ..CodexKey::default()
        }];
        sanitize_meta_keys(&mut keys);
        assert_eq!(keys.len(), 1);
        let key = &keys[0];
        assert_eq!(
            (
                key.api_key.as_str(),
                key.prefix.as_str(),
                key.base_url.as_str()
            ),
            ("LLM|key", "team", "https://meta.example.com/v1")
        );
        assert_eq!(
            key.headers,
            BTreeMap::from([("X-A".to_owned(), "1".to_owned())])
        );
        assert_eq!(key.excluded_models, strings(&["muse-1"]));
        assert!(!key.alpha_search);
    }

    // config_normalization.go: the rest of SanitizeGeminiKeys (no upstream
    // test).
    #[test]
    fn gemini_keys_are_cleaned_and_deduplicated() {
        let key = |api_key: &str, headers: &[(&str, &str)]| GeminiKey {
            api_key: api_key.to_owned(),
            prefix: " /team/ ".to_owned(),
            proxy_url: " socks5://proxy ".to_owned(),
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            excluded_models: strings(&[" Gemini-2.5-PRO ", "", "gemini-2.5-pro"]),
            ..GeminiKey::default()
        };
        let mut keys = vec![
            key(" k ", &[(" X-A ", " 1 "), ("X-B", " ")]),
            key("k", &[("X-A", "1")]),
            key("k", &[("X-A", "2")]),
        ];
        sanitize_gemini_keys(&mut keys);
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].api_key, "k");
        assert_eq!(keys[0].prefix, "team");
        assert_eq!(keys[0].proxy_url, "socks5://proxy");
        assert_eq!(
            keys[0].headers,
            BTreeMap::from([("X-A".to_owned(), "1".to_owned())])
        );
        assert_eq!(keys[0].excluded_models, strings(&["gemini-2.5-pro"]));
        assert_eq!(
            keys[1].headers,
            BTreeMap::from([("X-A".to_owned(), "2".to_owned())])
        );
    }

    // vertex_compat.go: SanitizeVertexCompatKeys (no upstream test).
    #[test]
    fn vertex_keys_need_a_key_and_models_need_both_names() {
        let model = |name: &str, alias: &str| VertexCompatModel {
            name: name.to_owned(),
            alias: alias.to_owned(),
            ..VertexCompatModel::default()
        };
        let mut keys = vec![
            VertexCompatKey {
                api_key: " ".to_owned(),
                base_url: "https://vertex.example.com".to_owned(),
                ..VertexCompatKey::default()
            },
            VertexCompatKey {
                api_key: " v ".to_owned(),
                base_url: " https://vertex.example.com ".to_owned(),
                prefix: "/p/".to_owned(),
                proxy_url: " direct ".to_owned(),
                headers: BTreeMap::from([(" X-A ".to_owned(), " 1 ".to_owned())]),
                models: vec![
                    model(" gemini-2.5-pro ", " vertex-pro "),
                    model("gemini-2.5-flash", " "),
                    model(" ", "alias"),
                ],
                excluded_models: strings(&[" A "]),
                ..VertexCompatKey::default()
            },
            // Same key and base URL: a duplicate, whatever the headers.
            VertexCompatKey {
                api_key: "v".to_owned(),
                base_url: "https://vertex.example.com".to_owned(),
                headers: BTreeMap::from([("X-B".to_owned(), "2".to_owned())]),
                ..VertexCompatKey::default()
            },
            VertexCompatKey {
                api_key: "v".to_owned(),
                ..VertexCompatKey::default()
            },
        ];
        sanitize_vertex_keys(&mut keys);
        assert_eq!(keys.len(), 2);
        let kept = &keys[0];
        assert_eq!(kept.api_key, "v");
        assert_eq!(kept.base_url, "https://vertex.example.com");
        assert_eq!(kept.prefix, "p");
        assert_eq!(kept.proxy_url, "direct");
        assert_eq!(
            kept.headers,
            BTreeMap::from([("X-A".to_owned(), "1".to_owned())])
        );
        assert_eq!(kept.models, [model("gemini-2.5-pro", "vertex-pro")]);
        assert_eq!(kept.excluded_models, strings(&["a"]));
        assert_eq!(keys[1].base_url, "");
    }

    // config_normalization.go: SanitizeOpenAICompatibility (no upstream test).
    #[test]
    fn openai_compatibility_without_base_url_is_dropped() {
        let mut providers = vec![
            OpenAiCompatibility {
                name: " gone ".to_owned(),
                base_url: " ".to_owned(),
                ..OpenAiCompatibility::default()
            },
            OpenAiCompatibility {
                name: " kept ".to_owned(),
                base_url: " https://example.invalid/v1 ".to_owned(),
                prefix: " /team/ ".to_owned(),
                headers: BTreeMap::from([
                    (" X-A ".to_owned(), " 1 ".to_owned()),
                    ("X-B".to_owned(), " ".to_owned()),
                ]),
                ..OpenAiCompatibility::default()
            },
        ];
        sanitize_openai_compatibility(&mut providers);
        assert_eq!(providers.len(), 1);
        let kept = &providers[0];
        assert_eq!(kept.name, "kept");
        assert_eq!(kept.base_url, "https://example.invalid/v1");
        assert_eq!(kept.prefix, "team");
        assert_eq!(
            kept.headers,
            BTreeMap::from([("X-A".to_owned(), "1".to_owned())])
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
