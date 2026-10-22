// Ported from CLIProxyAPI sdk/cliproxy/auth/api_key_model_capabilities.go and
// internal/modelconfig/model_info.go (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Bind the selected credential's configured upstream name before registry
//! lookup. Manager attempts use the config snapshot captured with their aliases;
//! direct executor calls use its config. No capability table
//! is copied onto the auth or maintained alongside the manager's aliases.

use open_ferry_core::auth::{Auth, AuthKind, AuthSource};
use open_ferry_core::config::{ClaudeModel, Config};
use open_ferry_core::manager::{ClaudeModelConfig, config_index, resolve_api_key_config};
use open_ferry_core::models::ModelInfo;
use open_ferry_core::registry::resolve_model_info;
use open_ferry_translate::go::equal_fold;

use super::thinking::parse_suffix;

pub(super) fn resolve(
    config: Option<&Config>,
    auth: &Auth,
    upstream: &str,
    route_model: &str,
    routing: Option<&ClaudeModelConfig>,
) -> Option<ModelInfo> {
    let (keys, cli) = routing
        .map(|snapshot| (&snapshot.claude_api_key, &snapshot.claude_cli))
        .or_else(|| config.map(|config| (&config.claude_api_key, &config.claude_cli)))?;
    let models = if auth.provider.trim().eq_ignore_ascii_case("claude-cli") {
        if auth.auth_source_kind() != Some(AuthSource::Config) {
            return None;
        }
        &cli.get(config_index(auth)?)?.models
    } else if auth.provider.trim().eq_ignore_ascii_case("claude")
        && auth.auth_kind() == Some(AuthKind::ApiKey)
    {
        &resolve_api_key_config(keys, auth)?.models
    } else {
        return None;
    };
    let upstream = upstream.trim();
    let requested = if route_model.trim().is_empty() {
        upstream
    } else {
        route_model.trim()
    };
    let requested = if auth.prefix.trim().is_empty() {
        requested
    } else {
        requested
            .strip_prefix(auth.prefix.trim())
            .and_then(|rest| rest.strip_prefix('/'))
            .unwrap_or(requested)
    };
    // Upstream compiles alias/name keys (each exact key, then its base).
    // Look up the attempt's exact route before its base, keeping the first
    // occurrence of an upstream name within each key.
    let mut routes: Vec<&ClaudeModel> = Vec::new();
    for candidate in lookup_candidates(requested) {
        let key = open_ferry_translate::go::to_lower(candidate);
        let mut by_key: Vec<&ClaudeModel> = Vec::new();
        for model in models {
            let name = model_name(model);
            if name.is_empty() {
                continue;
            }
            let alias = if model.alias.trim().is_empty() {
                name
            } else {
                model.alias.trim()
            };
            let matches = [alias, name].into_iter().any(|route| {
                lookup_candidates(route)
                    .into_iter()
                    .any(|value| open_ferry_translate::go::to_lower(value) == key)
            });
            if matches
                && !by_key
                    .iter()
                    .any(|prior| equal_fold(model_name(prior), name))
            {
                by_key.push(model);
            }
        }
        routes.extend(by_key);
    }
    let configured = routes
        .iter()
        .find(|model| equal_fold(model_name(model), upstream))
        .or_else(|| {
            routes.iter().find(|model| {
                let name = model_name(model);
                parse_suffix(name).1.is_none() && equal_fold(name, parse_suffix(upstream).0.trim())
            })
        })?;
    Some(resolve_model_info(
        model_name(configured),
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

fn lookup_candidates(model: &str) -> Vec<&str> {
    let model = model.trim();
    let base = parse_suffix(model).0.trim();
    if base.is_empty() || base == model {
        vec![model]
    } else {
        vec![model, base]
    }
}
