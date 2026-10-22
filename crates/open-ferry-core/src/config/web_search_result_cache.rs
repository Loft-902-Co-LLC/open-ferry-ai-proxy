// Ported from CLIProxyAPI internal/config/sdk_config.go (9d453bc0, v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Opt-in session web search caching. Both config layouts use the same client path.

use super::duration::parse_go_duration;
use serde::{Deserialize, Deserializer, de};
use std::fmt;
use std::time::Duration;

/// Settings at `client.codex.web-search-result-cache`.
/// Memory is bounded by `limit` entries of upstream-sized results; there is
/// no per-entry byte cap. Disabling or changing the policy purges the store.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodexWebSearchResultCacheConfig {
    pub enabled: bool,
    /// Go duration text or whole seconds, retained as text when saving.
    pub ttl: String,
    /// Zero uses the default 1024-entry limit.
    pub limit: i64,
}

// Use a map visitor: NodeDe deliberately ignores unknown struct fields for
// ordinary Go configs, but upstream's cache config explicitly rejects them.
impl<'de> Deserialize<'de> for CodexWebSearchResultCacheConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct CacheConfig;
        impl<'de> de::Visitor<'de> for CacheConfig {
            type Value = CodexWebSearchResultCacheConfig;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a web-search-result-cache mapping")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                use serde_json::Value;
                let mut config = Self::Value::default();
                let mut seen = std::collections::HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !seen.insert(key.clone()) {
                        return Err(de::Error::custom(format!(
                            "client.codex.web-search-result-cache: duplicate field {key}"
                        )));
                    }
                    let value = map.next_value::<Value>()?;
                    match (key.as_str(), value) {
                        ("enabled", Value::Null) => config.enabled = false,
                        ("enabled", Value::Bool(enabled)) => config.enabled = enabled,
                        ("enabled", _) => {
                            return Err(de::Error::custom(
                                "client.codex.web-search-result-cache.enabled must be a boolean",
                            ));
                        }
                        ("ttl", Value::Null) => config.ttl.clear(),
                        ("ttl", Value::String(ttl)) => config.ttl = ttl.trim().to_owned(),
                        ("ttl", Value::Number(ttl)) if ttl.as_i64().is_some() => {
                            config.ttl = ttl.to_string()
                        }
                        ("ttl", _) => {
                            return Err(de::Error::custom(
                                "client.codex.web-search-result-cache.ttl must be a duration string or integer seconds",
                            ));
                        }
                        ("limit", Value::Null) => config.limit = 0,
                        ("limit", Value::Number(limit)) if limit.as_i64().is_some() => {
                            config.limit = limit.as_i64().unwrap_or_default()
                        }
                        ("limit", _) => {
                            return Err(de::Error::custom(
                                "client.codex.web-search-result-cache.limit must be an integer",
                            ));
                        }
                        _ => {
                            return Err(de::Error::custom(format!(
                                "client.codex.web-search-result-cache: unknown field {key}"
                            )));
                        }
                    }
                }
                config.validate().map_err(de::Error::custom)?;
                Ok(config)
            }
        }
        deserializer.deserialize_any(CacheConfig)
    }
}

impl CodexWebSearchResultCacheConfig {
    pub const DEFAULT_TTL: Duration = Duration::from_secs(24 * 3600);
    pub const DEFAULT_LIMIT: usize = 1024;
    fn parsed_ttl(&self) -> Result<Duration, String> {
        let raw = self.ttl.trim();
        if raw.is_empty() {
            return Ok(Self::DEFAULT_TTL);
        }
        let nanos = if let Ok(seconds) = raw.parse::<i64>() {
            seconds.checked_mul(1_000_000_000)
        } else {
            parse_go_duration(raw)
        };
        match nanos {
            Some(0) => Ok(Self::DEFAULT_TTL),
            Some(nanos) if nanos > 0 => Ok(Duration::from_nanos(nanos as u64)),
            _ => Err("client.codex.web-search-result-cache.ttl must be a non-negative duration string or integer seconds within the duration range".to_owned()),
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        self.parsed_ttl()?;
        if self.limit < 0 {
            return Err(
                "client.codex.web-search-result-cache.limit must not be negative".to_owned(),
            );
        }
        Ok(())
    }
    pub fn effective_ttl(&self) -> Duration {
        self.parsed_ttl().unwrap_or(Self::DEFAULT_TTL)
    }
    pub fn effective_limit(&self) -> usize {
        usize::try_from(self.limit)
            .ok()
            .filter(|n| *n > 0)
            .unwrap_or(Self::DEFAULT_LIMIT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    #[test]
    fn defaults_and_duration_forms_in_both_layouts() {
        for layout in ["", "config-version: 8\n"] {
            for (raw, seconds) in [
                ("24h", 86400),
                ("1800", 1800),
                ("0", 86400),
                ("0s", 86400),
                ("'30m'", 1800),
            ] {
                let cfg = Config::parse(format!("{layout}client:\n  codex:\n    web-search-result-cache: {{enabled: true, ttl: {raw}, limit: 7}}\n")).unwrap();
                let cache = &cfg.client.codex.web_search_result_cache;
                assert!(cache.enabled);
                assert_eq!(cache.effective_ttl(), Duration::from_secs(seconds));
                assert_eq!(cache.effective_limit(), 7);
            }
        }
        for raw in ["null", "{enabled: null, ttl: null, limit: null}"] {
            let cfg = Config::parse(format!(
                "client: {{codex: {{web-search-result-cache: {raw}}}}}"
            ))
            .unwrap();
            assert_eq!(
                cfg.client.codex.web_search_result_cache,
                CodexWebSearchResultCacheConfig::default()
            );
        }
        let defaults = CodexWebSearchResultCacheConfig::default();
        assert!(!defaults.enabled);
        assert_eq!(defaults.effective_ttl(), Duration::from_secs(86400));
        assert_eq!(defaults.effective_limit(), 1024);
    }
    #[test]
    fn invalid_config_is_rejected() {
        for raw in [
            "{ttl: -1}",
            "{ttl: '-1h'}",
            "{ttl: never}",
            "{ttl: 9223372036854775807}",
            "{ttl: 1.5}",
            "{ttl: true}",
            "{limit: -1}",
            "{enabled: 1}",
            "{unknown: true}",
            "[]",
        ] {
            for prefix in ["", "config-version: 8\n"] {
                assert!(
                    Config::parse(format!(
                        "{prefix}client:\n  codex:\n    web-search-result-cache: {raw}\n"
                    ))
                    .is_err(),
                    "{prefix}{raw}"
                );
            }
        }
    }
}
