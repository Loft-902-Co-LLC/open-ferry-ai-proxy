// Ported from CLIProxyAPI sdk/cliproxy/auth/api_key_model_capabilities.go and
// internal/modelconfig/model_info.go (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Bind the selected credential's configured upstream name before registry
//! lookup. The executor's config is the source of truth; no capability table
//! is copied onto the auth or maintained alongside the manager's aliases.

use open_ferry_core::auth::{Auth, AuthKind, AuthSource};
use open_ferry_core::config::{ClaudeKey, ClaudeModel, Config};
use open_ferry_core::models::ModelInfo;
use open_ferry_core::registry::resolve_model_info;
use open_ferry_translate::go::equal_fold;

use super::thinking::parse_suffix;

pub(super) fn resolve(
    config: Option<&Config>,
    auth: &Auth,
    upstream: &str,
    requested: &str,
) -> Option<ModelInfo> {
    let config = config?;
    let models = if auth.provider.trim().eq_ignore_ascii_case("claude-cli") {
        if auth.auth_source_kind() != Some(AuthSource::Config) {
            return None;
        }
        &config.claude_cli.get(config_index(auth)?)?.models
    } else if auth.provider.trim().eq_ignore_ascii_case("claude")
        && auth.auth_kind() == Some(AuthKind::ApiKey)
    {
        &key_entry(config, auth)?.models
    } else {
        return None;
    };
    let upstream = upstream.trim();
    let requested = if requested.trim().is_empty() {
        upstream
    } else {
        requested.trim()
    };
    let requested = if auth.prefix.trim().is_empty() {
        requested
    } else {
        requested
            .strip_prefix(auth.prefix.trim())
            .and_then(|rest| rest.strip_prefix('/'))
            .unwrap_or(requested)
    };
    let candidates = models.iter().filter(|model| {
        // Two aliases may share an upstream name while declaring different
        // capabilities. Retain the manager's original route in that case.
        [&model.name, &model.alias].into_iter().any(|name| {
            equal_fold(
                parse_suffix(name.trim()).0.trim(),
                parse_suffix(requested).0.trim(),
            )
        })
    });
    let configured = candidates
        .clone()
        .find(|model| equal_fold(model_name(model), upstream))
        .or_else(|| {
            candidates.into_iter().find(|model| {
                let name = model_name(model);
                parse_suffix(name).1.is_none() && equal_fold(name, parse_suffix(upstream).0.trim())
            })
        })?;
    Some(resolve_model_info(
        upstream,
        "claude",
        configured.thinking.as_ref(),
    ))
}

fn model_name(model: &ClaudeModel) -> &str {
    if model.name.trim().is_empty() {
        model.alias.trim()
    } else {
        model.name.trim()
    }
}

fn config_index(auth: &Auth) -> Option<usize> {
    auth.attribute("config_index")?.trim().parse().ok()
}

/// The same config-index/credential/prefix/proxy precedence as the manager's
/// resolveAPIKeyConfig. An index left over after a reload cannot select a
/// different key. All returned models still come from the executor's config.
fn key_entry<'a>(config: &'a Config, auth: &Auth) -> Option<&'a ClaudeKey> {
    let key = auth.attribute("api_key").unwrap_or_default().trim();
    let base = auth.attribute("base_url").unwrap_or_default().trim();
    let matches = |entry: &ClaudeKey| {
        let (entry_key, entry_base) = (entry.api_key.trim(), entry.base_url.trim());
        if !key.is_empty() && !base.is_empty() {
            equal_fold(entry_key, key) && equal_fold(entry_base, base)
        } else if !key.is_empty() {
            equal_fold(entry_key, key) && (entry_base.is_empty() || equal_fold(entry_base, base))
        } else {
            !base.is_empty() && equal_fold(entry_base, base)
        }
    };
    let entries = &config.claude_api_key;
    if auth.auth_source_kind() == Some(AuthSource::Config)
        && let Some(entry) = config_index(auth).and_then(|index| entries.get(index))
        && matches(entry)
    {
        return Some(entry);
    }
    entries
        .iter()
        .find(|entry| {
            matches(entry)
                && equal_fold(entry.prefix.trim(), auth.prefix.trim())
                && equal_fold(entry.proxy_url.trim(), auth.proxy_url.trim())
        })
        .or_else(|| entries.iter().find(|entry| matches(entry)))
        .or_else(|| {
            (!key.is_empty())
                .then(|| {
                    entries
                        .iter()
                        .find(|entry| equal_fold(entry.api_key.trim(), key))
                })
                .flatten()
        })
}
