// Ported from CLIProxyAPI internal/runtime/executor/helps/vertex_openapi.go
// (ShouldExecuteVertexOpenAPIForRequest and its helpers, v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;
use open_ferry_core::auth::classification::AuthKind;
use open_ferry_core::auth::synthesizer::file::{ModelAlias, decode_model_aliases};
use open_ferry_core::config::VertexCompatKey;
use std::collections::HashSet;

pub(super) fn resolve_config<'a>(
    cfg: Option<&'a Config>,
    auth: &Auth,
) -> Option<&'a VertexCompatKey> {
    let keys = &cfg?.vertex_api_key;
    let key = auth.attribute("api_key").unwrap_or_default().trim();
    let base = auth.attribute("base_url").unwrap_or_default().trim();
    let matches = |entry: &&VertexCompatKey| {
        let (k, b) = (entry.api_key.trim(), entry.base_url.trim());
        if !key.is_empty() && !base.is_empty() {
            equal_fold(k, key) && equal_fold(b, base)
        } else if !key.is_empty() {
            equal_fold(k, key) && (b.is_empty() || equal_fold(b, base))
        } else {
            !base.is_empty() && equal_fold(b, base)
        }
    };
    auth.attribute("config_index")
        .and_then(|s| s.parse::<usize>().ok())
        .and_then(|index| keys.get(index))
        .filter(matches)
        .or_else(|| {
            keys.iter().find(|entry| {
                matches(entry)
                    && equal_fold(entry.prefix.trim(), auth.prefix.trim())
                    && equal_fold(entry.proxy_url.trim(), auth.proxy_url.trim())
            })
        })
        .or_else(|| keys.iter().find(matches))
        .or_else(|| {
            keys.iter()
                .find(|entry| !key.is_empty() && equal_fold(entry.api_key.trim(), key))
        })
}

fn rule(name: &str, alias: &str, openapi: Option<bool>) -> ModelAlias {
    ModelAlias {
        name: name.into(),
        alias: alias.into(),
        openapi,
        ..ModelAlias::default()
    }
}

fn from_value(value: Option<&Value>) -> Vec<ModelAlias> {
    match value {
        Some(Value::Array(items)) => decode_model_aliases(items),
        Some(value) => decode_model_aliases(std::slice::from_ref(value)),
        None => Vec::new(),
    }
}

fn from_attribute(auth: &Auth, key: &str) -> Vec<ModelAlias> {
    from_value(
        auth.attribute(key)
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .as_ref(),
    )
}

fn deduplicate(rules: Vec<ModelAlias>) -> Vec<ModelAlias> {
    let mut aliases = HashSet::new();
    let mut names = HashSet::new();
    rules
        .into_iter()
        .filter(|rule| {
            let (name, alias) = (rule.name.trim(), rule.alias.trim());
            if name.is_empty() && alias.is_empty() {
                return false;
            }
            if !alias.is_empty() && !equal_fold(name, alias) {
                aliases.insert(to_lower(alias))
            } else {
                rule.openapi.is_some() && names.insert(to_lower(name))
            }
        })
        .collect()
}

fn exact(value: &str, rule: &str) -> bool {
    !value.trim().is_empty() && !rule.trim().is_empty() && equal_fold(value.trim(), rule.trim())
}

fn base_match(value: &str, rule: &str) -> bool {
    !value.trim().is_empty()
        && !rule.trim().is_empty()
        && base_model(rule.trim()) == rule.trim()
        && equal_fold(
            base_model(value.trim()).trim(),
            base_model(rule.trim()).trim(),
        )
}

fn any_match(value: &str, rule: &str) -> bool {
    exact(value, rule) || base_match(value, rule)
}

fn match_rules(rules: &[ModelAlias], model: &str, requested: &str) -> Option<bool> {
    let target = if requested.is_empty() {
        model
    } else {
        requested
    };
    let consistent = |rule: &&ModelAlias| {
        rule.name.trim().is_empty()
            || model.trim().is_empty()
            || any_match(model, &rule.alias)
            || any_match(model, &rule.name)
    };
    // An alias with no override still shadows later aliases with that name.
    for compare in [exact, base_match] {
        if let Some(rule) = rules
            .iter()
            .find(|rule| compare(target, &rule.alias) && consistent(rule))
        {
            return rule.openapi;
        }
    }
    for compare in [exact, base_match] {
        if let Some(rule) = rules.iter().find(|rule| {
            rule.openapi.is_some()
                && (compare(model, &rule.name) || compare(requested, &rule.name))
                && (rule.alias.trim().is_empty()
                    || equal_fold(rule.alias.trim(), rule.name.trim())
                    || requested.is_empty()
                    || any_match(requested, &rule.alias)
                    || any_match(requested, &rule.name))
        }) {
            return rule.openapi;
        }
    }
    None
}

pub(in crate::gemini::vertex) fn should_execute(
    cfg: Option<&Config>,
    auth: &Auth,
    model: &str,
    options: &Options,
) -> bool {
    if is_imagen(base_model(model.trim())) {
        return false;
    }
    let requested = options.metadata.requested_model.trim();
    let requested = if requested.is_empty() {
        let original: Option<Value> = serde_json::from_slice(&options.original_request).ok();
        original
            .as_ref()
            .and_then(|v| v.get("model"))
            .map(|v| json::str_of(Some(v)))
            .unwrap_or_default()
    } else {
        requested.to_owned()
    };
    let mut requested = requested.trim();
    let prefix = if auth.prefix.trim().trim_matches('/').is_empty() {
        auth.attribute("prefix").unwrap_or_default()
    } else {
        &auth.prefix
    };
    let needle = format!("{}/", prefix.trim().trim_matches('/'));
    if needle != "/"
        && requested.len() > needle.len()
        && requested
            .get(..needle.len())
            .is_some_and(|p| equal_fold(p, &needle))
    {
        requested = requested[needle.len()..].trim();
    }
    let entry = resolve_config(cfg, auth);
    if let Some(entry) = entry {
        let rules: Vec<_> = entry
            .models
            .iter()
            .map(|m| rule(&m.name, &m.alias, m.openapi))
            .collect();
        if let Some(enabled) = match_rules(&rules, model, requested) {
            return enabled;
        }
    }
    let oauth = auth
        .metadata
        .get("service_account")
        .and_then(Value::as_object)
        .is_some_and(|v| !v.is_empty())
        || auth.auth_kind() != Some(AuthKind::ApiKey);
    let mut rules = Vec::new();
    for key in ["model_aliases", "model-aliases", "models"] {
        rules.extend(from_value(auth.metadata.get(key)));
    }
    rules.extend(from_attribute(auth, "vertex_model_openapi"));
    if oauth {
        rules.extend(from_attribute(auth, "model_aliases"));
        if let Some(aliases) = cfg.and_then(|cfg| cfg.oauth_model_alias.get("vertex")) {
            rules.extend(aliases.iter().map(|m| rule(&m.name, &m.alias, m.openapi)));
        }
    }
    if let Some(enabled) = match_rules(&deduplicate(rules), model, requested) {
        return enabled;
    }
    if oauth && let Some(settings) = cfg.and_then(|cfg| cfg.oauth_settings.get("vertex")) {
        let rules: Vec<_> = settings
            .iter()
            .map(|m| rule(&m.name, &m.alias, m.openapi))
            .collect();
        if let Some(enabled) = match_rules(&rules, model, requested) {
            return enabled;
        }
    }
    explicit_setting(auth)
        .or_else(|| entry.and_then(|e| e.openapi))
        .unwrap_or_else(|| is_partner(model))
}
