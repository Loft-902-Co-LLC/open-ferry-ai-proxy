// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_models.go
// (v8.0.42, MIT). https://github.com/router-for-me/CLIProxyAPI

use super::credential::{SOURCE_CONFIG, attribute, auth_source_kind};
use super::settings::ApiKeyEntry;
use super::text::{atoi, equal_fold};
use crate::auth::Auth;
use crate::config::ClaudeKey;

/// Credential fields shared by routing settings and the original config.
pub trait ApiKeyConfigEntry {
    /// Key, base URL, prefix and proxy URL, in that order.
    fn routing_identity(&self) -> (&str, &str, &str, &str);
}

impl ApiKeyConfigEntry for ApiKeyEntry {
    fn routing_identity(&self) -> (&str, &str, &str, &str) {
        (&self.api_key, &self.base_url, &self.prefix, &self.proxy_url)
    }
}
impl ApiKeyConfigEntry for ClaudeKey {
    fn routing_identity(&self) -> (&str, &str, &str, &str) {
        (&self.api_key, &self.base_url, &self.prefix, &self.proxy_url)
    }
}

/// The config entry of an API-key credential (upstream's
/// `resolveAPIKeyConfig`): its `config_index` entry, else one with its key
/// and base URL, prefix and proxy, else one with its key and base URL, else
/// one with its key.
pub fn resolve_api_key_config<'a, T: ApiKeyConfigEntry>(
    entries: &'a [T],
    auth: &Auth,
) -> Option<&'a T> {
    if entries.is_empty() {
        return None;
    }
    let attr_key = attribute(auth, "api_key");
    let attr_base = attribute(auth, "base_url");
    let matches_credentials = |entry: &T| {
        let (key, base, _, _) = entry.routing_identity();
        let cfg_key = key.trim();
        let cfg_base = base.trim();
        if !attr_key.is_empty() && !attr_base.is_empty() {
            return equal_fold(cfg_key, &attr_key) && equal_fold(cfg_base, &attr_base);
        }
        if !attr_key.is_empty() {
            return equal_fold(cfg_key, &attr_key)
                && (cfg_base.is_empty() || equal_fold(cfg_base, &attr_base));
        }
        !attr_base.is_empty() && equal_fold(cfg_base, &attr_base)
    };
    if auth_source_kind(auth) == SOURCE_CONFIG
        && let Some(entry) = config_index(auth).and_then(|index| entries.get(index))
        && matches_credentials(entry)
    {
        return Some(entry);
    }
    let found = entries.iter().find(|entry| {
        matches_credentials(entry)
            && equal_fold(entry.routing_identity().2.trim(), auth.prefix.trim())
            && equal_fold(entry.routing_identity().3.trim(), auth.proxy_url.trim())
    });
    if found.is_some() {
        return found;
    }
    if let Some(entry) = entries.iter().find(|entry| matches_credentials(entry)) {
        return Some(entry);
    }
    if !attr_key.is_empty() {
        return entries
            .iter()
            .find(|entry| equal_fold(entry.routing_identity().0.trim(), &attr_key));
    }
    None
}

/// The credential's `config_index` attribute, when it is a valid index.
pub fn config_index(auth: &Auth) -> Option<usize> {
    let raw = auth.attributes.get("config_index")?;
    let index = atoi(raw.trim())?;
    usize::try_from(index).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::manager::Settings;

    #[test]
    fn routing_and_capabilities_share_config_index_precedence() {
        let config = Config::parse(
            "claude-api-key: [{api-key: dummy, prefix: first}, {api-key: dummy, prefix: fallback}]",
        )
        .unwrap();
        let settings = Settings::from(&config);
        let mut auth = Auth {
            provider: "claude".into(),
            prefix: "fallback".into(),
            ..Auth::default()
        };
        auth.attributes
            .insert("source".into(), "config:claude[test]".into());
        auth.attributes.insert("api_key".into(), "dummy".into());
        for raw in ["-0", "+0", "00"] {
            auth.attributes.insert("config_index".into(), raw.into());
            assert_eq!(config_index(&auth), Some(0));
            assert_eq!(
                resolve_api_key_config(settings.api_key_entries("claude"), &auth)
                    .unwrap()
                    .prefix,
                "first"
            );
            assert_eq!(
                resolve_api_key_config(&config.claude_api_key, &auth)
                    .unwrap()
                    .prefix,
                "first"
            );
        }
    }
}
