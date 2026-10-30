// Ported from CLIProxyAPI sdk/cliproxy/auth/api_key_model_capabilities.go and
// internal/modelconfig/model_info.go (v8.0.42, MIT).
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
use open_ferry_translate::go::{equal_fold, equal_lower};

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
    // Walk the immutable attempt snapshot by reference. Keep exact/base key
    // order and first-name precedence without temporary lookup vectors or
    // owned lowercase strings. Lowercase-key equality must remain distinct
    // from EqualFold name deduplication (notably for Unicode model aliases).
    let mut routes = lookup_candidates(requested).flat_map(|candidate| {
        models.iter().enumerate().filter_map(move |(index, model)| {
            let matches = matches_route(model, candidate)
                && !models[..index].iter().any(|prior| {
                    equal_fold(model_name(prior), model_name(model))
                        && matches_route(prior, candidate)
                });
            matches.then_some(model)
        })
    });
    let configured = routes
        .clone()
        .find(|model| equal_fold(model_name(model), upstream))
        .or_else(|| {
            routes.find(|model| {
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

fn matches_route(model: &ClaudeModel, candidate: &str) -> bool {
    let name = model_name(model);
    if name.is_empty() {
        return false;
    }
    let alias = if model.alias.trim().is_empty() {
        name
    } else {
        model.alias.trim()
    };
    [alias, name]
        .into_iter()
        .any(|route| lookup_candidates(route).any(|value| equal_lower(value, candidate)))
}

fn lookup_candidates(model: &str) -> impl Iterator<Item = &str> + Clone {
    let model = model.trim();
    let base = parse_suffix(model).0.trim();
    std::iter::once(model).chain((!base.is_empty() && base != model).then_some(base))
}
