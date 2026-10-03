// Ported from CLIProxyAPI internal/config/config.go and
// internal/config/config_types.go (the fields the auth manager reads), and the
// strategy names in sdk/cliproxy/service_config.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The settings the manager runs with, as a plain struct made from the
//! config with [`Settings::from`].
//!
//! Deviations from upstream:
//! - Upstream reads these from its whole config; here they are their own
//!   struct, and the per-provider API-key lists are one map keyed by
//!   provider.
//! - Values are taken as given: trimming and dropping invalid rules
//!   (upstream's config sanitizing) is the config layer's job.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use super::text::go_lower;
use crate::config::{self, Config};

/// How the manager picks among ready credentials of the same priority.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RoutingStrategy {
    /// Take turns (upstream's `round-robin`, the default).
    #[default]
    RoundRobin,
    /// Use the first ready credential until it cools down (`fill-first`).
    FillFirst,
    /// Take turns in proportion to each credential's weight
    /// (`weighted-round-robin`).
    Weighted,
}

impl RoutingStrategy {
    /// The strategy a config value names: `fill-first`, `fillfirst` or `ff`;
    /// `weighted-round-robin`, `weightedroundrobin` or `wrr`; anything else
    /// is round-robin.
    pub fn parse(name: &str) -> Self {
        match go_lower(name.trim()).as_str() {
            "fill-first" | "fillfirst" | "ff" => Self::FillFirst,
            "weighted-round-robin" | "weightedroundrobin" | "wrr" => Self::Weighted,
            _ => Self::RoundRobin,
        }
    }
}

/// A client-facing model name and the upstream model it stands for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelAlias {
    /// The upstream model.
    pub name: String,
    /// The name clients use.
    pub alias: String,
    /// Whether responses show the alias in place of the upstream model
    /// (OAuth aliases only).
    pub force_mapping: bool,
}

/// What to do with an upstream error that matches a rule.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestScopedErrorRule {
    /// The upstream status the rule applies to.
    pub status: u16,
    /// Substrings of the error body, any of which matches.
    pub matches: Vec<String>,
    /// Regular expressions over the error body, any of which matches.
    pub match_regex: Vec<String>,
    /// `stop`, `stop-and-cooldown`, `continue` or `continue-and-cooldown`.
    pub action: String,
}

/// One configured API key of a built-in provider.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ApiKeyEntry {
    /// The key.
    pub api_key: String,
    /// The endpoint, or empty for the provider's default.
    pub base_url: String,
    /// The model prefix the key's credential uses.
    pub prefix: String,
    /// The proxy the key's credential uses.
    pub proxy_url: String,
    /// Model aliases for the key.
    pub models: Vec<ModelAlias>,
    /// Error rules for the key.
    pub request_scoped_errors: Vec<RequestScopedErrorRule>,
}

impl fmt::Debug for ApiKeyEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKeyEntry")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("prefix", &self.prefix)
            .field("proxy_url", &self.proxy_url)
            .field("models", &self.models)
            .field("request_scoped_errors", &self.request_scoped_errors)
            .finish()
    }
}

/// One configured OpenAI-compatible provider.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenAiCompat {
    /// The provider's name.
    pub name: String,
    /// Whether routing skips the provider.
    pub disabled: bool,
    /// Whether cooldowns are off (or on) for the provider's credentials,
    /// overriding the global setting.
    pub disable_cooling: Option<bool>,
    /// Model aliases; several entries with one alias form a pool the manager
    /// rotates through.
    pub models: Vec<ModelAlias>,
    /// Error rules for the provider.
    pub request_scoped_errors: Vec<RequestScopedErrorRule>,
}

/// The manager's settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// Extra rounds over the credentials after every one has failed
    /// (`request-retry`).
    pub request_retry: usize,
    /// The most credentials one request tries, or 0 for no limit
    /// (`max-retry-credentials`).
    pub max_retry_credentials: usize,
    /// The longest the manager waits for a credential to cool down before a
    /// retry round (`max-retry-interval`).
    pub max_retry_interval: Duration,
    /// Whether failures leave credentials usable instead of cooling them
    /// down (`disable-cooling`).
    pub disable_cooling: bool,
    /// The cooldown after a transient upstream error, in seconds: 0 for the
    /// default minute, negative for none
    /// (`transient-error-cooldown-seconds`).
    pub transient_error_cooldown_seconds: i64,
    /// How the manager picks among ready credentials (`routing.strategy`).
    pub routing_strategy: RoutingStrategy,
    /// How many credentials refresh at once, or 0 for 16
    /// (`auth-auto-refresh-workers`).
    pub refresh_workers: usize,
    /// OAuth model aliases by channel, such as `codex` or `claude`
    /// (`oauth-model-alias`).
    pub oauth_model_alias: BTreeMap<String, Vec<ModelAlias>>,
    /// Error rules for OAuth credentials by provider, lower case
    /// (`oauth-request-scoped-errors`).
    pub oauth_request_scoped_errors: BTreeMap<String, Vec<RequestScopedErrorRule>>,
    /// API keys by provider: `claude`, `codex`, `xai`, `meta`, `gemini`,
    /// `gemini-interactions` and `vertex` (upstream's `claude-api-key` and
    /// the like). A credential made from one carries its index in the
    /// `config_index` attribute.
    pub api_keys: BTreeMap<String, Vec<ApiKeyEntry>>,
    /// OpenAI-compatible providers (`openai-compatibility`).
    pub openai_compatibility: Vec<OpenAiCompat>,
}

impl From<&Config> for Settings {
    /// The settings in `config`. Negative counts and intervals are 0, as
    /// upstream's `SetRetryConfig` makes them.
    fn from(config: &Config) -> Self {
        let count = |value: i64| usize::try_from(value).unwrap_or(0);
        let rules = |rules: &[config::RequestScopedErrorRule]| {
            rules.iter().map(RequestScopedErrorRule::from).collect()
        };
        let mut api_keys = BTreeMap::new();
        api_keys.insert(
            "claude".to_owned(),
            config
                .claude_api_key
                .iter()
                .map(|key| ApiKeyEntry {
                    api_key: key.api_key.clone(),
                    base_url: key.base_url.clone(),
                    prefix: key.prefix.clone(),
                    proxy_url: key.proxy_url.clone(),
                    models: key
                        .models
                        .iter()
                        .map(|model| ModelAlias {
                            name: model.name.clone(),
                            alias: model.alias.clone(),
                            force_mapping: model.force_mapping,
                        })
                        .collect(),
                    request_scoped_errors: rules(&key.request_scoped_errors),
                })
                .collect(),
        );
        api_keys.insert(
            "codex".to_owned(),
            config
                .codex_api_key
                .iter()
                .map(|key| ApiKeyEntry {
                    api_key: key.api_key.clone(),
                    base_url: key.base_url.clone(),
                    prefix: key.prefix.clone(),
                    proxy_url: key.proxy_url.clone(),
                    models: key
                        .models
                        .iter()
                        .map(|model| ModelAlias {
                            name: model.name.clone(),
                            alias: model.alias.clone(),
                            force_mapping: model.force_mapping,
                        })
                        .collect(),
                    request_scoped_errors: rules(&key.request_scoped_errors),
                })
                .collect(),
        );
        Self {
            request_retry: count(config.request_retry),
            max_retry_credentials: count(config.max_retry_credentials),
            max_retry_interval: Duration::from_secs(
                u64::try_from(config.max_retry_interval).unwrap_or(0),
            ),
            disable_cooling: config.disable_cooling,
            transient_error_cooldown_seconds: config.transient_error_cooldown_seconds,
            routing_strategy: RoutingStrategy::parse(&config.routing.strategy),
            refresh_workers: count(config.auth_auto_refresh_workers),
            oauth_model_alias: config
                .oauth_model_alias
                .iter()
                .map(|(channel, aliases)| {
                    let aliases = aliases
                        .iter()
                        .map(|alias| ModelAlias {
                            name: alias.name.clone(),
                            alias: alias.alias.clone(),
                            force_mapping: alias.force_mapping,
                        })
                        .collect();
                    (channel.clone(), aliases)
                })
                .collect(),
            oauth_request_scoped_errors: config
                .oauth_request_scoped_errors
                .iter()
                .map(|(provider, list)| (provider.clone(), rules(list)))
                .collect(),
            api_keys,
            openai_compatibility: config
                .openai_compatibility
                .iter()
                .map(|compat| OpenAiCompat {
                    name: compat.name.clone(),
                    disabled: compat.disabled,
                    disable_cooling: compat.disable_cooling,
                    models: compat
                        .models
                        .iter()
                        .map(|model| ModelAlias {
                            name: model.name.clone(),
                            alias: model.alias.clone(),
                            force_mapping: model.force_mapping,
                        })
                        .collect(),
                    request_scoped_errors: rules(&compat.request_scoped_errors),
                })
                .collect(),
        }
    }
}

impl From<&config::RequestScopedErrorRule> for RequestScopedErrorRule {
    /// The rule; a status no response can have never matches.
    fn from(rule: &config::RequestScopedErrorRule) -> Self {
        Self {
            status: u16::try_from(rule.status).unwrap_or(0),
            matches: rule.matches.clone(),
            match_regex: rule.match_regexr.clone(),
            action: rule.action.clone(),
        }
    }
}

impl Settings {
    /// The API-key entries of `provider`, which must be lower case.
    pub(crate) fn api_key_entries(&self, provider: &str) -> &[ApiKeyEntry] {
        self.api_keys.get(provider).map_or(&[], Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strategy_names() {
        assert_eq!(RoutingStrategy::parse(" FF "), RoutingStrategy::FillFirst);
        assert_eq!(RoutingStrategy::parse("wrr"), RoutingStrategy::Weighted);
        assert_eq!(
            RoutingStrategy::parse("weightedroundrobin"),
            RoutingStrategy::Weighted
        );
        assert_eq!(
            RoutingStrategy::parse("random"),
            RoutingStrategy::RoundRobin
        );
    }

    #[test]
    fn settings_come_from_the_config() {
        let config = Config::parse(
            r#"
request-retry: -2
max-retry-credentials: 3
max-retry-interval: 45
disable-cooling: true
auth-auto-refresh-workers: 4
routing:
  strategy: fill-first
oauth-model-alias:
  codex:
    - name: gpt-5
      alias: g5
      force-mapping: true
oauth-request-scoped-errors:
  claude:
    - status: 400
      match: ["quota"]
      match-regexr: ["^x"]
      action: stop
claude-api-key:
  - api-key: sk-1
    base-url: https://example.test
    models:
      - name: claude-x
        alias: cx
openai-compatibility:
  - name: Kimi
    disabled: true
    disable-cooling: false
    base-url: https://compat.example.test/v1
    api-key-entries:
      - api-key: compat-key
    models:
      - name: kimi-k2
        alias: k2
        force-mapping: true
    request-scoped-errors:
      - status: 400
        match: ["too long"]
        action: stop
"#,
        )
        .unwrap();
        let settings = Settings::from(&config);
        assert_eq!(settings.request_retry, 0);
        assert_eq!(settings.max_retry_credentials, 3);
        assert_eq!(settings.max_retry_interval, Duration::from_secs(45));
        assert!(settings.disable_cooling);
        assert_eq!(settings.refresh_workers, 4);
        assert_eq!(settings.routing_strategy, RoutingStrategy::FillFirst);
        assert_eq!(
            settings.oauth_model_alias["codex"],
            [ModelAlias {
                name: "gpt-5".into(),
                alias: "g5".into(),
                force_mapping: true,
            }]
        );
        assert_eq!(
            settings.oauth_request_scoped_errors["claude"],
            [RequestScopedErrorRule {
                status: 400,
                matches: vec!["quota".into()],
                match_regex: vec!["^x".into()],
                action: "stop".into(),
            }]
        );
        let claude = settings.api_key_entries("claude");
        assert_eq!(claude.len(), 1);
        assert_eq!(claude[0].api_key, "sk-1");
        assert_eq!(claude[0].base_url, "https://example.test");
        assert_eq!(claude[0].models[0].alias, "cx");
        assert!(settings.api_key_entries("codex").is_empty());
        assert_eq!(
            settings.openai_compatibility,
            [OpenAiCompat {
                name: "Kimi".into(),
                disabled: true,
                disable_cooling: Some(false),
                models: vec![ModelAlias {
                    name: "kimi-k2".into(),
                    alias: "k2".into(),
                    force_mapping: true,
                }],
                request_scoped_errors: vec![RequestScopedErrorRule {
                    status: 400,
                    matches: vec!["too long".into()],
                    match_regex: Vec::new(),
                    action: "stop".into(),
                }],
            }]
        );
    }

    #[test]
    fn debug_hides_api_keys() {
        let entry = ApiKeyEntry {
            api_key: "sk-secret".into(),
            ..ApiKeyEntry::default()
        };
        assert!(!format!("{entry:?}").contains("sk-secret"));
    }
}
