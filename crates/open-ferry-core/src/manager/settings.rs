// Ported from CLIProxyAPI internal/config/config.go and
// internal/config/config_types.go (the fields the auth manager reads), and the
// strategy names, routingRuntimeState and normalizedRoutingRuntimeState in
// sdk/cliproxy/service_config.go (v8.0.20, MIT).
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
use crate::config::{self, Config, Redacted, RedactedUrl};

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

/// The routing settings whose change replaces the selector, normalized
/// (upstream's `routingRuntimeState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RoutingState {
    pub(crate) strategy: RoutingStrategy,
    pub(crate) session_affinity: bool,
    /// The binding TTL: an hour for none, at least a second.
    pub(crate) session_affinity_ttl: Duration,
    /// Whether subagents may take their parent's credential: yes unless
    /// session affinity is on and says no.
    pub(crate) session_affinity_subagents: bool,
}

impl RoutingState {
    /// The routing state `settings` give (upstream's
    /// `normalizedRoutingRuntimeState`).
    pub(crate) fn of(settings: &Settings) -> Self {
        let ttl = settings.session_affinity_ttl;
        Self {
            strategy: settings.routing_strategy,
            session_affinity: settings.session_affinity,
            session_affinity_ttl: if ttl.is_zero() {
                Duration::from_secs(60 * 60)
            } else {
                ttl.max(Duration::from_secs(1))
            },
            session_affinity_subagents: !settings.session_affinity
                || settings.session_affinity_subagents.unwrap_or(true),
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
            .field("base_url", &RedactedUrl(&self.base_url))
            .field("prefix", &self.prefix)
            .field("proxy_url", &Redacted(&self.proxy_url))
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
    /// Whether a conversation stays on the credential that served it
    /// (`routing.session-affinity`).
    pub session_affinity: bool,
    /// How long a conversation's binding lives after its last use, or zero
    /// for an hour; under a second counts as a second
    /// (`routing.session-affinity-ttl`).
    pub session_affinity_ttl: Duration,
    /// Whether a subagent may take its parent's credential, or `None` for
    /// yes; read only with session affinity on
    /// (`routing.session-affinity-subagents`).
    pub session_affinity_subagents: Option<bool>,
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
    /// `config_index` attribute. Not upstream's: `claude-cli` holds
    /// open-ferry's `claude-cli` entries, every one in the config's order,
    /// with no key or base URL; only their prefix and models are read.
    pub api_keys: BTreeMap<String, Vec<ApiKeyEntry>>,
    /// OpenAI-compatible providers (`openai-compatibility`).
    pub openai_compatibility: Vec<OpenAiCompat>,
}

/// The binding TTL `text` gives: a positive Go duration, or zero (an hour)
/// for anything else (upstream's `normalizedRoutingRuntimeState`).
fn affinity_ttl(text: &str) -> Duration {
    config::parse_go_duration(text.trim())
        .and_then(|nanos| u64::try_from(nanos).ok())
        .map_or(Duration::ZERO, Duration::from_nanos)
}

impl From<&Config> for Settings {
    /// The settings in `config`. Negative counts and intervals are 0, as
    /// upstream's `SetRetryConfig` makes them.
    fn from(config: &Config) -> Self {
        let count = |value: i64| usize::try_from(value).unwrap_or(0);
        let rules = |rules: &[config::RequestScopedErrorRule]| {
            rules.iter().map(RequestScopedErrorRule::from).collect()
        };
        let gemini = |keys: &[config::GeminiKey]| -> Vec<ApiKeyEntry> {
            keys.iter()
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
                .collect()
        };
        let codex = |keys: &[config::CodexKey]| -> Vec<ApiKeyEntry> {
            keys.iter()
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
                .collect()
        };
        let mut api_keys = BTreeMap::new();
        api_keys.insert("gemini".to_owned(), gemini(&config.gemini_api_key));
        api_keys.insert(
            "gemini-interactions".to_owned(),
            gemini(&config.interactions_api_key),
        );
        api_keys.insert(
            "vertex".to_owned(),
            config
                .vertex_api_key
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
                    request_scoped_errors: Vec::new(),
                })
                .collect(),
        );
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
        api_keys.insert("codex".to_owned(), codex(&config.codex_api_key));
        api_keys.insert("xai".to_owned(), codex(&config.xai_api_key));
        api_keys.insert("meta".to_owned(), codex(&config.meta_api_key));
        api_keys.insert(
            "claude-cli".to_owned(),
            config
                .claude_cli
                .iter()
                .map(|entry| ApiKeyEntry {
                    api_key: String::new(),
                    base_url: String::new(),
                    prefix: entry.prefix.clone(),
                    proxy_url: String::new(),
                    models: entry
                        .models
                        .iter()
                        .map(|model| ModelAlias {
                            name: model.name.clone(),
                            alias: model.alias.clone(),
                            force_mapping: model.force_mapping,
                        })
                        .collect(),
                    request_scoped_errors: Vec::new(),
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
            session_affinity: config.routing.session_affinity,
            session_affinity_ttl: affinity_ttl(&config.routing.session_affinity_ttl),
            session_affinity_subagents: config.routing.session_affinity_subagents,
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
        assert!(settings.api_key_entries("gemini").is_empty());

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
        let keyed = Config::parse(
            "gemini-api-key:\n  - api-key: g\n    prefix: team\n    proxy-url: direct\n    \
             models: [{name: gemini-2.5-pro, alias: pro, force-mapping: true}]\n    \
             request-scoped-errors: [{status: 400, match: [long], action: stop}]\n\
             vertex-api-key:\n  - api-key: v\n    base-url: https://vertex.example.test\n    \
             models: [{name: gemini-2.5-flash, alias: flash}]\n",
        )
        .unwrap();
        let keyed = Settings::from(&keyed);
        assert_eq!(
            keyed.api_key_entries("gemini"),
            [ApiKeyEntry {
                api_key: "g".into(),
                prefix: "team".into(),
                proxy_url: "direct".into(),
                models: vec![ModelAlias {
                    name: "gemini-2.5-pro".into(),
                    alias: "pro".into(),
                    force_mapping: true,
                }],
                request_scoped_errors: vec![RequestScopedErrorRule {
                    status: 400,
                    matches: vec!["long".into()],
                    match_regex: Vec::new(),
                    action: "stop".into(),
                }],
                ..ApiKeyEntry::default()
            }]
        );
        let vertex = keyed.api_key_entries("vertex");
        assert_eq!(
            (vertex[0].api_key.as_str(), vertex[0].base_url.as_str()),
            ("v", "https://vertex.example.test")
        );
        assert_eq!(vertex[0].models[0].alias, "flash");
    }

    // Not upstream's: the interactions, xAI and Meta keys, which upstream's
    // manager reads from its config (resolveAPIKeyConfig and
    // requestScopedErrorRulesForAuth).
    #[test]
    fn interactions_xai_and_meta_keys_come_from_the_config() {
        let config = Config::parse(concat!(
            "interactions-api-key:\n  - api-key: i\n",
            "    models: [{name: gemini-2.5-flash, alias: native-flash}]\n",
            "xai-api-key:\n  - api-key: x\n    base-url: https://xai.example.test\n",
            "    request-scoped-errors: [{status: 400, match: [long], action: stop}]\n",
            "meta-api-key:\n  - api-key: m\n    prefix: team\n",
            "    models: [{name: muse-spark-1.3, alias: muse, force-mapping: true}]\n",
        ))
        .unwrap();
        let settings = Settings::from(&config);
        let interactions = settings.api_key_entries("gemini-interactions");
        assert_eq!(interactions.len(), 1);
        assert_eq!(interactions[0].api_key, "i");
        assert_eq!(interactions[0].models[0].alias, "native-flash");
        let xai = settings.api_key_entries("xai");
        assert_eq!(
            (xai[0].api_key.as_str(), xai[0].base_url.as_str()),
            ("x", "https://xai.example.test")
        );
        assert_eq!(xai[0].request_scoped_errors[0].action, "stop");
        let meta = settings.api_key_entries("meta");
        assert_eq!(meta[0].prefix, "team");
        // The loader defaults Meta's base URL.
        assert_eq!(meta[0].base_url, "https://api.meta.ai/v1");
        assert_eq!(
            meta[0].models,
            [ModelAlias {
                name: "muse-spark-1.3".into(),
                alias: "muse".into(),
                force_mapping: true,
            }]
        );
        assert!(settings.api_key_entries("gemini").is_empty());
    }

    #[test]
    fn debug_hides_api_keys() {
        let entry = ApiKeyEntry {
            api_key: "sk-secret".into(),
            base_url: "https://gateway.example/v1?key=sk-secret".into(),
            proxy_url: "http://user:sk-secret@proxy.example:8080".into(),
            ..ApiKeyEntry::default()
        };
        let shown = format!("{entry:?}");
        assert!(!shown.contains("sk-secret"), "{shown}");
        assert!(
            shown.contains(r#""https://gateway.example/v1?<redacted>""#),
            "{shown}"
        );
    }

    /// The routing state of a config whose `routing` section is `routing`.
    fn routing(routing: &str) -> RoutingState {
        let config = Config::parse(format!("routing: {routing}\n")).expect("config");
        RoutingState::of(&Settings::from(&config))
    }

    // Ports TestServiceApplyConfigRuntimePreservesSelectorForUnchangedRouting
    // and TestBuilderPreservesInitialSelectorForSameRouting
    // (service_executionregistry_test.go): the manager keeps its bindings
    // while the routing state is equal, as upstream keeps its selector.
    #[test]
    fn same_routing_written_differently_is_unchanged() {
        let initial =
            routing("{strategy: fill-first, session-affinity: true, session-affinity-ttl: 1h}");
        assert_eq!(
            routing(
                r#"{strategy: " FILLFIRST ", session-affinity: true, session-affinity-ttl: 60m}"#
            ),
            initial
        );
        assert_ne!(
            routing("{strategy: round-robin, session-affinity: true, session-affinity-ttl: 1h}"),
            initial
        );
    }

    // Ports TestServiceApplyConfigRuntimeSessionAffinitySubagentsChangeRecreatesSelector
    // and TestServiceApplyConfigRuntimeSessionAffinityDisabledSubagentsChangeIsNoOp
    // (service_executionregistry_test.go).
    #[test]
    fn subagent_setting_counts_only_with_session_affinity() {
        assert_ne!(
            routing(
                "{session-affinity: true, session-affinity-ttl: 1h, session-affinity-subagents: true}"
            ),
            routing(
                "{session-affinity: true, session-affinity-ttl: 1h, session-affinity-subagents: false}"
            )
        );
        assert_eq!(
            routing(
                "{session-affinity: false, session-affinity-ttl: 1h, session-affinity-subagents: true}"
            ),
            routing(
                "{session-affinity: false, session-affinity-ttl: 1h, session-affinity-subagents: false}"
            )
        );
    }

    // Not upstream's: the TTL as upstream's normalizedRoutingRuntimeState
    // reads it.
    #[test]
    fn session_affinity_ttl() {
        const HOUR: Duration = Duration::from_secs(60 * 60);
        let cases = [
            ("", HOUR),
            (" 30m ", Duration::from_secs(30 * 60)),
            ("2h30m", Duration::from_secs(150 * 60)),
            ("500ms", Duration::from_secs(1)),
            ("0s", HOUR),
            ("-5m", HOUR),
            ("30", HOUR),
            ("soon", HOUR),
        ];
        for (ttl, want) in cases {
            let state = routing(&format!(
                r#"{{session-affinity: true, session-affinity-ttl: "{ttl}"}}"#
            ));
            assert!(state.session_affinity);
            assert_eq!(state.session_affinity_ttl, want, "{ttl:?}");
        }
        let state = routing("{}");
        assert!(!state.session_affinity);
        assert_eq!(state.session_affinity_ttl, HOUR);
        assert!(state.session_affinity_subagents);
    }
}
