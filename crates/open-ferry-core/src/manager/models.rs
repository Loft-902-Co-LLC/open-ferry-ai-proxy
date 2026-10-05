// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_models.go,
// sdk/cliproxy/auth/oauth_model_alias.go, isConfiguredModelRoutingAuth in
// sdk/cliproxy/auth/api_key_model_capabilities.go, OpenAICompatibleProviderKey
// in internal/util/provider.go and SanitizeOAuthModelAlias in
// internal/config/config_normalization.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Which upstream model a credential is asked for: the route model with the
//! credential's prefix stripped, then mapped through OAuth aliases, the
//! configured API-key aliases, or an OpenAI-compatible provider's model pool.
//!
//! Deviations from upstream:
//! - The API-key alias table is compiled per call from the credential and the
//!   current settings, where upstream caches it per credential ID and
//!   rebuilds it on every change.
//! - Home's force-mapping and upstream-model attributes aren't read; Home
//!   isn't ported.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use super::credential::{KIND_API_KEY, SOURCE_CONFIG, attribute, auth_kind, auth_source_kind};
use super::settings::{ApiKeyEntry, ModelAlias, OpenAiCompat, Settings};
use super::text::{atoi, canonical_model_key, equal_fold, go_lower, parse_suffix};
use crate::auth::Auth;
pub(crate) use crate::auth::compat::openai_compatible_provider_key;
use crate::auth::json::{decode_field, fold_values};

/// The upstream model an alias resolves to, and how responses should name
/// it (upstream's `OAuthModelAliasResult`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AliasResult {
    /// The upstream model, or empty without a mapping.
    pub(crate) upstream_model: String,
    /// Whether responses should show `original_alias` as the model.
    pub(crate) force_mapping: bool,
    /// The model clients asked for.
    pub(crate) original_alias: String,
}

#[derive(Clone, Debug)]
struct OAuthAliasEntry {
    upstream_model: String,
    config_alias: String,
    force_mapping: bool,
}

/// The OAuth aliases by channel and lower-case alias (upstream's
/// `oauthModelAliasTable`).
#[derive(Clone, Debug, Default)]
pub(crate) struct OAuthAliasTable {
    reverse: HashMap<String, HashMap<String, OAuthAliasEntry>>,
}

impl OAuthAliasTable {
    /// Upstream's `compileOAuthModelAliasTable`. Within a channel the first
    /// entry for an alias wins.
    pub(crate) fn compile<'a>(
        aliases: impl IntoIterator<Item = (&'a String, &'a Vec<ModelAlias>)>,
    ) -> Self {
        let mut reverse = HashMap::new();
        for (raw_channel, entries) in aliases {
            let channel = go_lower(raw_channel.trim());
            if channel.is_empty() || entries.is_empty() {
                continue;
            }
            let mut rev: HashMap<String, OAuthAliasEntry> = HashMap::new();
            for entry in entries {
                let name = entry.name.trim();
                let alias = entry.alias.trim();
                if name.is_empty() || alias.is_empty() || equal_fold(name, alias) {
                    continue;
                }
                rev.entry(go_lower(alias))
                    .or_insert_with(|| OAuthAliasEntry {
                        upstream_model: name.to_owned(),
                        config_alias: alias.to_owned(),
                        force_mapping: entry.force_mapping,
                    });
            }
            if !rev.is_empty() {
                reverse.insert(channel, rev);
            }
        }
        Self { reverse }
    }
}

/// What model resolution reads: the settings and the compiled OAuth aliases.
#[derive(Clone, Copy)]
pub(crate) struct Resolver<'a> {
    pub(crate) settings: &'a Settings,
    pub(crate) oauth: &'a OAuthAliasTable,
}

/// The route model without the credential's `prefix/` (upstream's
/// `rewriteModelForAuth`).
pub(crate) fn rewrite_model_for_auth(model: &str, auth: &Auth) -> String {
    if model.is_empty() {
        return String::new();
    }
    let prefix = auth.prefix.trim();
    if prefix.is_empty() {
        return model.to_owned();
    }
    model
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(model)
        .to_owned()
}

/// Whether the credential's models come from the config: an API key, or a
/// config-made OpenAI-compatible entry (upstream's
/// `isConfiguredModelRoutingAuth`).
pub(crate) fn is_configured_model_routing_auth(auth: &Auth) -> bool {
    if auth_kind(auth) == KIND_API_KEY {
        return true;
    }
    auth_source_kind(auth) == SOURCE_CONFIG && !attribute(auth, "compat_name").is_empty()
}

/// Upstream's `isConfiguredOpenAICompatAuth`.
pub(crate) fn is_configured_openai_compat_auth(auth: &Auth) -> bool {
    if !is_configured_model_routing_auth(auth) {
        return false;
    }
    equal_fold(auth.provider.trim(), "openai-compatibility")
        || !attribute(auth, "compat_name").is_empty()
}

fn openai_compat_provider_key(auth: &Auth) -> String {
    let provider_key = attribute(auth, "provider_key");
    if !provider_key.is_empty() {
        return openai_compatible_provider_key(&provider_key);
    }
    let compat_name = attribute(auth, "compat_name");
    if !compat_name.is_empty() {
        return openai_compatible_provider_key(&compat_name);
    }
    openai_compatible_provider_key(&auth.provider)
}

/// The key a credential's model pool rotates under (upstream's
/// `openAICompatModelPoolKey`).
pub(crate) fn openai_compat_model_pool_key(auth: &Auth, requested_model: &str) -> String {
    let mut base = parse_suffix(requested_model).0.trim();
    if base.is_empty() {
        base = requested_model.trim();
    }
    format!(
        "{}|{}|{}",
        go_lower(auth.id.trim()),
        openai_compat_provider_key(auth),
        go_lower(base)
    )
}

/// `values` rotated left by `offset` (upstream's `rotateStrings`).
pub(crate) fn rotate_strings(mut values: Vec<String>, offset: usize) -> Vec<String> {
    if values.len() <= 1 || offset == 0 {
        return values;
    }
    let len = values.len();
    values.rotate_left(offset % len);
    values
}

/// `resolved` with the request's thinking suffix, unless it has its own
/// (upstream's `preserveResolvedModelSuffix`).
fn preserve_resolved_model_suffix(resolved: &str, request_suffix: Option<&str>) -> String {
    let resolved = resolved.trim();
    if resolved.is_empty() {
        return String::new();
    }
    if parse_suffix(resolved).1.is_some() {
        return resolved.to_owned();
    }
    match request_suffix {
        Some(raw) if !raw.is_empty() => format!("{resolved}({raw})"),
        _ => resolved.to_owned(),
    }
}

/// Upstream's `preserveRequestedModelSuffix`.
pub(crate) fn preserve_requested_model_suffix(requested_model: &str, resolved: &str) -> String {
    preserve_resolved_model_suffix(resolved, parse_suffix(requested_model).1)
}

/// The names an alias lookup tries: the model, then its base without the
/// suffix (upstream's `modelAliasLookupCandidates`).
fn model_alias_lookup_candidates(requested_model: &str) -> (Option<&str>, &str, Vec<&str>) {
    let requested = requested_model.trim();
    if requested.is_empty() {
        return (None, "", Vec::new());
    }
    let (name, suffix) = parse_suffix(requested);
    let base = if name.is_empty() { requested } else { name };
    let mut candidates = vec![requested];
    if base != requested {
        candidates.push(base);
    }
    (suffix, name, candidates)
}

/// Every upstream model an alias maps to, in config order, else the model
/// itself if the config names it (upstream's
/// `resolveModelAliasPoolFromConfigModels`).
pub(crate) fn resolve_model_alias_pool_from_config_models(
    requested_model: &str,
    models: &[ModelAlias],
) -> Vec<String> {
    let requested = requested_model.trim();
    if requested.is_empty() || models.is_empty() {
        return Vec::new();
    }
    let (suffix, _, candidates) = model_alias_lookup_candidates(requested);
    for candidate in &candidates {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for model in models {
            let name = model.name.trim();
            let alias = model.alias.trim();
            if candidate.is_empty() || alias.is_empty() || !equal_fold(alias, candidate) {
                continue;
            }
            let resolved = if name.is_empty() { candidate } else { name };
            let resolved = preserve_resolved_model_suffix(resolved, suffix);
            let key = go_lower(resolved.trim());
            if key.is_empty() || !seen.insert(key) {
                continue;
            }
            out.push(resolved);
        }
        if !out.is_empty() {
            return out;
        }
    }
    for candidate in &candidates {
        for model in models {
            let name = model.name.trim();
            if candidate.is_empty() || name.is_empty() || !equal_fold(name, candidate) {
                continue;
            }
            return vec![preserve_resolved_model_suffix(name, suffix)];
        }
    }
    Vec::new()
}

/// Upstream's `resolveModelAliasFromConfigModels`.
pub(super) fn resolve_model_alias_from_config_models(
    requested_model: &str,
    models: &[ModelAlias],
) -> String {
    resolve_model_alias_pool_from_config_models(requested_model, models)
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// The first alias entry matching the model (upstream's
/// `resolveModelAliasResultFromConfigModels` and
/// `resolveUpstreamModelFromAliases`).
pub(super) fn resolve_model_alias_result_from_config_models(
    requested_model: &str,
    models: &[ModelAlias],
) -> AliasResult {
    let requested = requested_model.trim();
    if requested.is_empty() || models.is_empty() {
        return AliasResult::default();
    }
    let (suffix, name, candidates) = model_alias_lookup_candidates(requested);
    let base_model = if name.is_empty() { requested } else { name };
    for candidate in candidates {
        let key = candidate.trim();
        if key.is_empty() {
            continue;
        }
        for model in models {
            let original = model.name.trim();
            let alias = model.alias.trim();
            if original.is_empty() || alias.is_empty() || !equal_fold(alias, key) {
                continue;
            }
            if equal_fold(original, base_model) {
                if !model.force_mapping {
                    return AliasResult::default();
                }
                return AliasResult {
                    upstream_model: preserve_resolved_model_suffix(original, suffix),
                    force_mapping: true,
                    original_alias: alias.to_owned(),
                };
            }
            let original_alias = if model.force_mapping {
                alias.to_owned()
            } else {
                requested.to_owned()
            };
            return AliasResult {
                upstream_model: preserve_resolved_model_suffix(original, suffix),
                force_mapping: model.force_mapping,
                original_alias,
            };
        }
    }
    AliasResult::default()
}

/// The OAuth alias channel for a provider and credential kind (upstream's
/// `OAuthModelAliasChannel`): none for API keys or Gemini, else the
/// provider.
pub(crate) fn oauth_model_alias_channel(provider: &str, kind: &str) -> String {
    let provider = go_lower(provider.trim());
    let kind = go_lower(kind.trim());
    if matches!(kind.as_str(), "apikey" | "api_key" | "api-key") {
        return String::new();
    }
    if provider == "gemini" {
        return String::new();
    }
    provider
}

fn model_alias_channel(auth: &Auth) -> String {
    oauth_model_alias_channel(&auth.provider, auth_kind(auth))
}

/// A string struct field as Go's decoder fills it from an object's keys
/// (see [`decode_field`]).
fn decode_string(object: &Map<String, Value>, name: &str) -> Option<String> {
    decode_field(fold_values(object, name), String::new(), |value| {
        value.as_str().map(str::to_owned)
    })
}

/// A bool struct field as Go's decoder fills it from an object's keys.
fn decode_bool(object: &Map<String, Value>, name: &str) -> Option<bool> {
    decode_field(fold_values(object, name), false, Value::as_bool)
}

/// The per-credential OAuth aliases in the `model_aliases` attribute,
/// sanitized as the config's are (upstream's
/// `OAuthModelAliasesFromAttributes`). A malformed list is none.
fn oauth_model_aliases_from_attributes(auth: &Auth) -> Vec<ModelAlias> {
    let raw = attribute(auth, "model_aliases");
    if raw.is_empty() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let Value::Array(items) = value else {
        return Vec::new();
    };
    let mut aliases = Vec::with_capacity(items.len());
    for item in &items {
        match item {
            Value::Null => aliases.push(ModelAlias::default()),
            Value::Object(object) => {
                let (Some(name), Some(alias), Some(_fork), Some(_display), Some(force)) = (
                    decode_string(object, "name"),
                    decode_string(object, "alias"),
                    decode_bool(object, "fork"),
                    decode_string(object, "display-name"),
                    decode_bool(object, "force-mapping"),
                ) else {
                    return Vec::new();
                };
                aliases.push(ModelAlias {
                    name,
                    alias,
                    force_mapping: force,
                });
            }
            _ => return Vec::new(),
        }
    }
    sanitize_oauth_model_aliases(aliases)
}

/// Trimmed, without blank or self aliases, each alias once (upstream's
/// `SanitizeOAuthModelAlias` for one channel).
fn sanitize_oauth_model_aliases(aliases: Vec<ModelAlias>) -> Vec<ModelAlias> {
    let mut seen = HashSet::new();
    let mut clean = Vec::new();
    for entry in aliases {
        let name = entry.name.trim();
        let alias = entry.alias.trim();
        if name.is_empty() || alias.is_empty() || equal_fold(name, alias) {
            continue;
        }
        if !seen.insert(go_lower(alias)) {
            continue;
        }
        clean.push(ModelAlias {
            name: name.to_owned(),
            alias: alias.to_owned(),
            force_mapping: entry.force_mapping,
        });
    }
    clean
}

impl Resolver<'_> {
    /// Upstream's `resolveOAuthModelAliasWithResult`: the credential's own
    /// aliases, then the config's for its channel.
    pub(crate) fn resolve_oauth_model_alias_with_result(
        &self,
        auth: &Auth,
        requested_model: &str,
    ) -> AliasResult {
        let channel = model_alias_channel(auth);
        if channel.is_empty() {
            return AliasResult::default();
        }
        let own = oauth_model_aliases_from_attributes(auth);
        let result = resolve_model_alias_result_from_config_models(requested_model, &own);
        if !result.upstream_model.is_empty() {
            return result;
        }
        self.resolve_upstream_model_from_alias_table(requested_model, &channel)
    }

    fn resolve_upstream_model_from_alias_table(
        &self,
        requested_model: &str,
        channel: &str,
    ) -> AliasResult {
        if channel.is_empty() {
            return AliasResult::default();
        }
        let (suffix, base_model, candidates) = model_alias_lookup_candidates(requested_model);
        let Some(rev) = self.oauth.reverse.get(channel) else {
            return AliasResult::default();
        };
        for candidate in candidates {
            let key = go_lower(candidate.trim());
            if key.is_empty() {
                continue;
            }
            let Some(entry) = rev.get(&key) else {
                continue;
            };
            let target = entry.upstream_model.as_str();
            if target.is_empty() {
                continue;
            }
            if equal_fold(target, base_model) {
                if !entry.force_mapping {
                    return AliasResult::default();
                }
                return AliasResult {
                    upstream_model: preserve_resolved_model_suffix(target, suffix),
                    force_mapping: true,
                    original_alias: entry.config_alias.trim().to_owned(),
                };
            }
            let upstream_model = if parse_suffix(target).1.is_some() {
                target.to_owned()
            } else {
                match suffix {
                    Some(raw) if !raw.is_empty() => format!("{target}({raw})"),
                    _ => target.to_owned(),
                }
            };
            let original_alias = if entry.force_mapping {
                entry.config_alias.trim().to_owned()
            } else {
                requested_model.to_owned()
            };
            return AliasResult {
                upstream_model,
                force_mapping: entry.force_mapping,
                original_alias,
            };
        }
        AliasResult::default()
    }

    /// The OAuth alias's upstream model, else the requested one (upstream's
    /// `applyOAuthModelAlias`).
    pub(crate) fn apply_oauth_model_alias(&self, auth: &Auth, requested_model: &str) -> String {
        let upstream = self
            .resolve_oauth_model_alias_with_result(auth, requested_model)
            .upstream_model;
        if upstream.is_empty() {
            requested_model.to_owned()
        } else {
            upstream
        }
    }

    /// Upstream's `applyOAuthModelAliasWithResult`.
    pub(super) fn apply_oauth_model_alias_with_result(
        &self,
        auth: &Auth,
        requested_model: &str,
    ) -> AliasResult {
        let result = self.resolve_oauth_model_alias_with_result(auth, requested_model);
        if result.upstream_model.is_empty() {
            return AliasResult {
                upstream_model: requested_model.to_owned(),
                ..AliasResult::default()
            };
        }
        result
    }

    /// The model selection uses for a credential: the route model without the
    /// prefix, through OAuth aliases (upstream's `selectionModelForAuth`).
    pub(crate) fn selection_model_for_auth(&self, auth: &Auth, route_model: &str) -> String {
        let mut requested = rewrite_model_for_auth(route_model, auth);
        if requested.trim().is_empty() {
            requested = route_model.trim().to_owned();
        }
        let resolved = self.apply_oauth_model_alias(auth, &requested);
        if resolved.trim().is_empty() {
            return requested;
        }
        resolved
    }

    /// Upstream's `selectionModelKeyForAuth`.
    pub(crate) fn selection_model_key_for_auth(&self, auth: &Auth, route_model: &str) -> String {
        canonical_model_key(&self.selection_model_for_auth(auth, route_model))
    }

    /// The model a call's cooldown state is kept under (upstream's
    /// `stateModelForExecution`).
    pub(crate) fn state_model_for_execution(
        &self,
        auth: &Auth,
        route_model: &str,
        upstream_model: &str,
        pooled: bool,
    ) -> String {
        let state_model = execution_result_model(route_model, upstream_model, pooled);
        let selection_model = self.selection_model_for_auth(auth, route_model);
        if canonical_model_key(&selection_model) == canonical_model_key(upstream_model)
            && !selection_model.trim().is_empty()
        {
            return upstream_model.trim().to_owned();
        }
        state_model
    }

    /// The upstream models to try with a credential, whether they form a
    /// pool, and the alias behind them (upstream's
    /// `executionModelCandidatesWithAlias`). `pool_offset` gives the rotation
    /// for a pool's key and size.
    pub(crate) fn execution_model_candidates_with_alias(
        &self,
        auth: &Auth,
        route_model: &str,
        pool_offset: impl FnOnce(&str, usize) -> usize,
    ) -> (Vec<String>, bool, AliasResult) {
        let requested = rewrite_model_for_auth(route_model, auth);
        let alias_result = if is_configured_model_routing_auth(auth) {
            self.resolve_api_key_model_alias_with_result(auth, &requested)
        } else {
            self.apply_oauth_model_alias_with_result(auth, &requested)
        };
        let upstream_model = execution_alias_pool_model(auth, &requested, &alias_result);
        let pool = self.resolve_openai_compat_upstream_model_pool(auth, &upstream_model);
        let candidates = if pool.len() > 1 {
            let offset = pool_offset(
                &openai_compat_model_pool_key(auth, &upstream_model),
                pool.len(),
            );
            rotate_strings(pool, offset)
        } else if !pool.is_empty() {
            pool
        } else {
            let mut resolved = self.apply_api_key_model_alias(auth, &upstream_model);
            if resolved.trim().is_empty() {
                resolved = upstream_model;
            }
            vec![resolved]
        };
        let pooled = candidates.len() > 1;
        (candidates, pooled, alias_result)
    }

    /// The upstream models to try with a credential (upstream's
    /// `executionModelCandidates`, without the Home dispatcher's model):
    /// the route model without the credential's prefix, through its OAuth
    /// alias, then its OpenAI-compatible pool or API-key alias. `pool_offset`
    /// gives the rotation for a pool's key and size.
    pub(crate) fn execution_model_candidates(
        &self,
        auth: &Auth,
        route_model: &str,
        pool_offset: impl FnOnce(&str, usize) -> usize,
    ) -> Vec<String> {
        let requested = rewrite_model_for_auth(route_model, auth);
        let requested = self.apply_oauth_model_alias(auth, &requested);
        let pool = self.resolve_openai_compat_upstream_model_pool(auth, &requested);
        if pool.len() > 1 {
            let offset = pool_offset(&openai_compat_model_pool_key(auth, &requested), pool.len());
            return rotate_strings(pool, offset);
        }
        if !pool.is_empty() {
            return pool;
        }
        let resolved = self.apply_api_key_model_alias(auth, &requested);
        if resolved.trim().is_empty() {
            vec![requested]
        } else {
            vec![resolved]
        }
    }

    /// The alias behind the upstream model one attempt used (upstream's
    /// `resolveAttemptAliasResult` and `resolveModelAliasResultForUpstream`):
    /// for an API key or OpenAI-compatible credential, the configured entry
    /// that maps to it; else `fallback`.
    pub(crate) fn resolve_attempt_alias_result(
        &self,
        auth: &Auth,
        route_model: &str,
        upstream_model: &str,
        fallback: &AliasResult,
    ) -> AliasResult {
        if !is_configured_model_routing_auth(auth) {
            return fallback.clone();
        }
        let requested = rewrite_model_for_auth(route_model, auth);
        let requested = requested.trim();
        let upstream = upstream_model.trim();
        let mut result = AliasResult::default();
        if !requested.is_empty() && !upstream.is_empty() {
            let suffix = parse_suffix(requested).1;
            let filtered: Vec<ModelAlias> = self
                .configured_model_alias_entries(auth)
                .iter()
                .filter(|model| {
                    let name = model.name.trim();
                    !name.is_empty()
                        && equal_fold(&preserve_resolved_model_suffix(name, suffix), upstream)
                })
                .cloned()
                .collect();
            if !filtered.is_empty() {
                result = resolve_model_alias_result_from_config_models(requested, &filtered);
            }
        }
        if result.upstream_model.trim().is_empty() {
            return fallback.clone();
        }
        if result.force_mapping
            && fallback.force_mapping
            && !fallback.original_alias.trim().is_empty()
        {
            result.original_alias = fallback.original_alias.clone();
        }
        result
    }

    /// Upstream's `resolveAPIKeyModelAliasWithResult`.
    pub(super) fn resolve_api_key_model_alias_with_result(
        &self,
        auth: &Auth,
        requested_model: &str,
    ) -> AliasResult {
        let requested = requested_model.trim();
        if requested.is_empty() {
            return AliasResult::default();
        }
        let models = self.configured_model_alias_entries(auth);
        let fallback = || AliasResult {
            upstream_model: requested.to_owned(),
            ..AliasResult::default()
        };
        if models.is_empty() {
            return fallback();
        }
        let result = resolve_model_alias_result_from_config_models(requested, models);
        if result.upstream_model.trim().is_empty() {
            return fallback();
        }
        result
    }

    /// The configured model aliases of the credential's API key or
    /// OpenAI-compatible provider (upstream's `configuredModelAliasEntries`).
    pub(super) fn configured_model_alias_entries(&self, auth: &Auth) -> &[ModelAlias] {
        let provider = go_lower(auth.provider.trim());
        match provider.as_str() {
            "gemini" | "gemini-interactions" | "claude" | "codex" | "xai" | "vertex" | "meta" => {
                resolve_api_key_config(self.settings.api_key_entries(&provider), auth)
                    .map_or(&[], |entry| entry.models.as_slice())
            }
            _ => {
                let provider_key = attribute(auth, "provider_key");
                let compat_name = attribute(auth, "compat_name");
                if !compat_name.is_empty()
                    || equal_fold(auth.provider.trim(), "openai-compatibility")
                {
                    resolve_openai_compat_config_for_auth(
                        self.settings,
                        auth,
                        &provider_key,
                        &compat_name,
                    )
                    .map_or(&[], |entry| entry.models.as_slice())
                } else {
                    &[]
                }
            }
        }
    }

    /// The upstream models of an OpenAI-compatible provider's alias pool
    /// (upstream's `resolveOpenAICompatUpstreamModelPool`).
    fn resolve_openai_compat_upstream_model_pool(
        &self,
        auth: &Auth,
        requested_model: &str,
    ) -> Vec<String> {
        if !is_configured_openai_compat_auth(auth) {
            return Vec::new();
        }
        let requested = requested_model.trim();
        if requested.is_empty() {
            return Vec::new();
        }
        let provider_key = attribute(auth, "provider_key");
        let compat_name = attribute(auth, "compat_name");
        match resolve_openai_compat_config_for_auth(
            self.settings,
            auth,
            &provider_key,
            &compat_name,
        ) {
            Some(entry) => resolve_model_alias_pool_from_config_models(requested, &entry.models),
            None => Vec::new(),
        }
    }

    /// The API key's upstream model for the requested one, else the requested
    /// model (upstream's `applyAPIKeyModelAliasWithRouting`).
    pub(super) fn apply_api_key_model_alias(&self, auth: &Auth, requested_model: &str) -> String {
        if auth_kind(auth) != KIND_API_KEY {
            return requested_model.to_owned();
        }
        let requested = requested_model.trim();
        if requested.is_empty() {
            return String::new();
        }
        if let Some(resolved) = self.lookup_api_key_upstream_model(auth, requested) {
            return resolved;
        }
        let provider = go_lower(auth.provider.trim());
        let upstream = match provider.as_str() {
            "gemini" | "gemini-interactions" | "claude" | "codex" | "xai" | "vertex" | "meta" => {
                resolve_api_key_config(self.settings.api_key_entries(&provider), auth)
                    .map(|entry| resolve_model_alias_from_config_models(requested, &entry.models))
                    .unwrap_or_default()
            }
            _ => {
                let provider_key = attribute(auth, "provider_key");
                let compat_name = attribute(auth, "compat_name");
                if compat_name.is_empty()
                    && !equal_fold(auth.provider.trim(), "openai-compatibility")
                {
                    String::new()
                } else {
                    resolve_openai_compat_config_for_auth(
                        self.settings,
                        auth,
                        &provider_key,
                        &compat_name,
                    )
                    .map(|entry| resolve_model_alias_from_config_models(requested, &entry.models))
                    .unwrap_or_default()
                }
            }
        };
        if upstream.is_empty() {
            requested.to_owned()
        } else {
            upstream
        }
    }

    /// The fast path of the API-key alias lookup: the credential's compiled
    /// table (upstream's `lookupAPIKeyUpstreamModel`).
    pub(super) fn lookup_api_key_upstream_model(
        &self,
        auth: &Auth,
        requested: &str,
    ) -> Option<String> {
        if auth.id.trim().is_empty() || !is_configured_model_routing_auth(auth) {
            return None;
        }
        let by_alias = compile_api_key_model_alias(self.configured_model_alias_entries(auth));
        if by_alias.is_empty() {
            return None;
        }
        let first = go_lower(requested);
        let base = go_lower(parse_suffix(requested).0.trim());
        let mut keys = vec![first.clone()];
        if !base.is_empty() && base != first {
            keys.push(base);
        }
        keys.iter().find_map(|key| {
            let resolved = by_alias.get(key)?.trim();
            (!resolved.is_empty()).then(|| preserve_requested_model_suffix(requested, resolved))
        })
    }
}

/// The alias table of one credential (upstream's
/// `compileAPIKeyModelAliasForModels`): each alias, alias base, name and name
/// base maps to the name, the first entry winning.
fn compile_api_key_model_alias(models: &[ModelAlias]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut add = |key: &str, name: &str| {
        let key = go_lower(key.trim());
        if !key.is_empty() {
            out.entry(key).or_insert_with(|| name.to_owned());
        }
    };
    for model in models {
        let alias = model.alias.trim();
        let name = model.name.trim();
        if alias.is_empty() || name.is_empty() {
            continue;
        }
        add(alias, name);
        add(parse_suffix(alias).0, name);
        add(name, name);
        add(parse_suffix(name).0, name);
    }
    out
}

/// Upstream's `executionResultModel`.
pub(crate) fn execution_result_model(
    route_model: &str,
    upstream_model: &str,
    pooled: bool,
) -> String {
    if pooled && !upstream_model.trim().is_empty() {
        return upstream_model.trim().to_owned();
    }
    if !route_model.trim().is_empty() {
        return route_model.trim().to_owned();
    }
    upstream_model.trim().to_owned()
}

/// Upstream's `executionAliasPoolModel`.
fn execution_alias_pool_model(auth: &Auth, requested_model: &str, alias: &AliasResult) -> String {
    if is_configured_model_routing_auth(auth) && !requested_model.trim().is_empty() {
        return requested_model.to_owned();
    }
    if !alias.upstream_model.trim().is_empty() {
        return alias.upstream_model.clone();
    }
    requested_model.to_owned()
}

/// The config entry of an API-key credential (upstream's
/// `resolveAPIKeyConfig`): its `config_index` entry, else one with its key
/// and base URL, prefix and proxy, else one with its key and base URL, else
/// one with its key.
pub(crate) fn resolve_api_key_config<'a>(
    entries: &'a [ApiKeyEntry],
    auth: &Auth,
) -> Option<&'a ApiKeyEntry> {
    if entries.is_empty() {
        return None;
    }
    let attr_key = attribute(auth, "api_key");
    let attr_base = attribute(auth, "base_url");
    let matches_credentials = |entry: &ApiKeyEntry| {
        let cfg_key = entry.api_key.trim();
        let cfg_base = entry.base_url.trim();
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
            && equal_fold(entry.prefix.trim(), auth.prefix.trim())
            && equal_fold(entry.proxy_url.trim(), auth.proxy_url.trim())
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
            .find(|entry| equal_fold(entry.api_key.trim(), &attr_key));
    }
    None
}

/// The credential's `config_index` attribute, when it is a valid index.
pub(crate) fn config_index(auth: &Auth) -> Option<usize> {
    let raw = auth.attributes.get("config_index")?;
    let index = atoi(raw.trim())?;
    usize::try_from(index).ok()
}

/// The OpenAI-compatible provider of a credential (upstream's
/// `resolveOpenAICompatConfigForAuth`): its enabled `config_index` entry when
/// it came from the config, else by name.
pub(crate) fn resolve_openai_compat_config_for_auth<'a>(
    settings: &'a Settings,
    auth: &Auth,
    provider_key: &str,
    compat_name: &str,
) -> Option<&'a OpenAiCompat> {
    if auth_source_kind(auth) == SOURCE_CONFIG
        && let Some(entry) =
            config_index(auth).and_then(|index| settings.openai_compatibility.get(index))
        && !entry.disabled
    {
        return Some(entry);
    }
    resolve_openai_compat_config(settings, provider_key, compat_name, &auth.provider)
}

/// The first enabled OpenAI-compatible provider named by the compat name,
/// provider key or provider (upstream's `resolveOpenAICompatConfig`).
pub(crate) fn resolve_openai_compat_config<'a>(
    settings: &'a Settings,
    provider_key: &str,
    compat_name: &str,
    auth_provider: &str,
) -> Option<&'a OpenAiCompat> {
    let candidates: Vec<&str> = [compat_name, provider_key, auth_provider]
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect();
    settings.openai_compatibility.iter().find(|compat| {
        !compat.disabled
            && candidates
                .iter()
                .any(|candidate| equal_fold(candidate, &compat.name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn alias(name: &str, alias: &str, force: bool) -> ModelAlias {
        ModelAlias {
            name: name.into(),
            alias: alias.into(),
            force_mapping: force,
        }
    }

    fn oauth_auth(provider: &str) -> Auth {
        let mut auth = Auth {
            id: "a".into(),
            provider: provider.into(),
            ..Auth::default()
        };
        auth.metadata.insert("email".into(), "x@y".into());
        auth
    }

    #[test]
    fn oauth_aliases_keep_suffixes() {
        let mut aliases = BTreeMap::new();
        aliases.insert(
            "Codex".to_owned(),
            vec![
                alias("gpt-5", "fast", false),
                alias("gpt-5-mini", "fast", false),
                alias("gpt-5(high)", "deep", true),
            ],
        );
        let table = OAuthAliasTable::compile(&aliases);
        let settings = Settings::default();
        let resolver = Resolver {
            settings: &settings,
            oauth: &table,
        };
        let auth = oauth_auth("codex");
        assert_eq!(
            resolver.apply_oauth_model_alias(&auth, "fast(low)"),
            "gpt-5(low)"
        );
        assert_eq!(resolver.apply_oauth_model_alias(&auth, "FAST"), "gpt-5");
        let deep = resolver.resolve_oauth_model_alias_with_result(&auth, "deep(low)");
        assert_eq!(deep.upstream_model, "gpt-5(high)");
        assert!(deep.force_mapping);
        assert_eq!(deep.original_alias, "deep");
        assert_eq!(resolver.apply_oauth_model_alias(&auth, "other"), "other");
        // Gemini and API keys have no OAuth channel.
        assert_eq!(oauth_model_alias_channel("gemini", "oauth"), "");
        assert_eq!(oauth_model_alias_channel("codex", "api-key"), "");
    }

    #[test]
    fn per_credential_aliases_win() {
        let table = OAuthAliasTable::default();
        let settings = Settings::default();
        let resolver = Resolver {
            settings: &settings,
            oauth: &table,
        };
        let mut auth = oauth_auth("claude");
        auth.attributes.insert(
            "model_aliases".into(),
            r#"[{"Name":"claude-x","alias":"cx"},{"name":"y","alias":"cx"}]"#.into(),
        );
        assert_eq!(resolver.apply_oauth_model_alias(&auth, "cx"), "claude-x");
        auth.attributes.insert(
            "model_aliases".into(),
            r#"[{"name":1,"alias":"cx"}]"#.into(),
        );
        assert_eq!(resolver.apply_oauth_model_alias(&auth, "cx"), "cx");
    }

    #[test]
    fn api_key_aliases_and_pools() {
        let mut settings = Settings::default();
        settings.api_keys.insert(
            "claude".into(),
            vec![ApiKeyEntry {
                api_key: "k1".into(),
                models: vec![alias("claude-real", "short", false)],
                ..ApiKeyEntry::default()
            }],
        );
        settings.openai_compatibility.push(OpenAiCompat {
            name: "pool".into(),
            models: vec![alias("m1", "pooled", false), alias("m2", "pooled", false)],
            ..OpenAiCompat::default()
        });
        let table = OAuthAliasTable::default();
        let resolver = Resolver {
            settings: &settings,
            oauth: &table,
        };
        let mut claude = Auth {
            id: "c".into(),
            provider: "claude".into(),
            prefix: "team".into(),
            ..Auth::default()
        };
        claude.attributes.insert("api_key".into(), "k1".into());
        let (models, pooled, _) =
            resolver.execution_model_candidates_with_alias(&claude, "team/short(8k)", |_, _| 0);
        assert_eq!(models, vec!["claude-real(8k)"]);
        assert!(!pooled);

        let mut compat = Auth {
            id: "p".into(),
            provider: "pool".into(),
            ..Auth::default()
        };
        compat.attributes.insert("api_key".into(), "k".into());
        compat
            .attributes
            .insert("compat_name".into(), "pool".into());
        let (models, pooled, _) =
            resolver.execution_model_candidates_with_alias(&compat, "pooled", |key, size| {
                assert_eq!(key, "p|openai-compatible-pool|pooled");
                assert_eq!(size, 2);
                1
            });
        assert_eq!(models, vec!["m2", "m1"]);
        assert!(pooled);
        assert_eq!(
            resolver.state_model_for_execution(&compat, "pooled", "m2", true),
            "m2"
        );
        assert_eq!(
            resolver.state_model_for_execution(&claude, "team/short", "claude-real", false),
            "team/short"
        );
    }

    #[test]
    fn api_key_config_lookup_order() {
        let entries = vec![
            ApiKeyEntry {
                api_key: "k".into(),
                base_url: "https://a".into(),
                ..ApiKeyEntry::default()
            },
            ApiKeyEntry {
                api_key: "K".into(),
                prefix: "p".into(),
                ..ApiKeyEntry::default()
            },
        ];
        let mut auth = Auth {
            prefix: "p".into(),
            ..Auth::default()
        };
        auth.attributes.insert("api_key".into(), "k".into());
        // The key matches both; the prefix picks the second.
        assert!(std::ptr::eq(
            resolve_api_key_config(&entries, &auth).unwrap(),
            &entries[1]
        ));
        auth.attributes
            .insert("source".into(), "config:claude[0]".into());
        auth.attributes.insert("config_index".into(), "0".into());
        auth.attributes
            .insert("base_url".into(), "https://a".into());
        assert!(std::ptr::eq(
            resolve_api_key_config(&entries, &auth).unwrap(),
            &entries[0]
        ));
        assert_eq!(rewrite_model_for_auth("pp/x", &auth), "pp/x");
        assert_eq!(rewrite_model_for_auth("p/x", &auth), "x");
        assert_eq!(
            openai_compatible_provider_key(" Foo "),
            "openai-compatible-foo"
        );
        assert_eq!(
            rotate_strings(vec!["a".into(), "b".into(), "c".into()], 4),
            ["b", "c", "a"]
        );
    }
}
