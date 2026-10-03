// Ported from CLIProxyAPI sdk/cliproxy/service_models.go and
// registerResolvedModelsForAuth in sdk/cliproxy/service_executors.go,
// sdk/cliproxy/auth/classification.go and the alias helpers in
// sdk/cliproxy/auth/oauth_model_alias.go, ResolveOAuthModelSetting in
// internal/config/config_types.go, SanitizeOAuthModelAlias in
// internal/config/config_normalization.go, internal/modelconfig/model_info.go,
// ParseSuffix in internal/thinking/suffix.go, the per-account settings read
// in internal/watcher/synthesizer/file.go and helpers.go, and the plan claim
// in internal/auth/codex/jwt_parser.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Which models a credential serves, and registering them.
//!
//! A Claude credential serves the catalog's Claude models, or the models its
//! config entry lists. A Codex account serves its ChatGPT plan's models; a
//! Codex API key serves the models its config entry lists, or the Pro models.
//! Then exclusions take models out (`*` matches any text), aliases rename
//! models or, with `fork`, add names for them, settings set context windows,
//! and a prefix namespaces them, as in `team-a/gpt-5`.
//!
//! The config settings involved come in a [`RegistrationRules`].
//!
//! Deviations from upstream:
//! - Only Claude and Codex credentials get models; a credential of any other
//!   provider is unregistered. OpenAI-compatible providers, plugin models and
//!   Antigravity capability probing aren't ported.
//! - Upstream skips a credential its credential manager no longer holds, or
//!   no longer holds enabled; that check is the caller's. Unregistering a
//!   legacy runtime client ID isn't ported.
//! - Upstream reads a credential's plan type, excluded models and model
//!   aliases from attributes its file loader sets from the credential's file.
//!   When an attribute is missing, they are read from the metadata here, as
//!   that loader reads them: the plan type from `plan_type` or the
//!   `id_token` claims; the excluded models from `excluded_models`, merged
//!   with the global list; the aliases from `model_aliases`.
//! - Of an `id_token`'s claims, only the plan type is checked; upstream also
//!   rejects claims whose other fields have the wrong type.
//! - The rules' maps are looked up by channel, also matching a key that
//!   differs only in case or surrounding whitespace. Their values are used
//!   as given: the config loader is expected to clean them as upstream's
//!   does.
//! - An alias's `force-mapping` isn't kept; it matters only when routing a
//!   request.
//! - The catalog's native capabilities aren't copied to configured models;
//!   they aren't ported.

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use base64::Engine as _;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use open_ferry_translate::go;
use serde_json::Value;

use super::definitions::{CodexPlan, StaticCatalog};
use super::{ModelRegistry, equal_fold, json};
use crate::auth::Auth;
use crate::models::{ModelInfo, ThinkingSupport};

/// The kind of a credential that is an API key.
pub const AUTH_KIND_API_KEY: &str = "apikey";
/// The kind of a credential that is an OAuth account.
pub const AUTH_KIND_OAUTH: &str = "oauth";

/// A credential from the config file.
pub const AUTH_SOURCE_CONFIG: &str = "config";
/// A credential from a file.
pub const AUTH_SOURCE_FILE: &str = "file";
/// A credential from a git store.
pub const AUTH_SOURCE_GIT: &str = "git";
/// A credential that lives only in memory.
pub const AUTH_SOURCE_MEMORY: &str = "memory";
/// A credential from an object store.
pub const AUTH_SOURCE_OBJECT_STORE: &str = "objectstore";
/// A credential from a Postgres store.
pub const AUTH_SOURCE_POSTGRES: &str = "postgres";

/// The plan a Codex account has when its token doesn't say.
const DEFAULT_CODEX_PLAN_TYPE: &str = "free";

/// The claims object of an OpenAI token that holds the ChatGPT plan.
const OPENAI_AUTH_CLAIM: &str = "https://api.openai.com/auth";

/// Go's `base64.URLEncoding`: padded, and lenient about the unused bits of
/// the last character.
const JWT_BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
);

/// The config settings that decide a credential's models.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegistrationRules {
    /// List models only with the credential's prefix (`force-model-prefix`).
    pub force_model_prefix: bool,
    /// Models OAuth credentials don't serve, by provider
    /// (`oauth-excluded-models`). `*` matches any text.
    pub oauth_excluded_models: BTreeMap<String, Vec<String>>,
    /// Other names for OAuth credentials' models, by channel
    /// (`oauth-model-alias`).
    pub oauth_model_alias: BTreeMap<String, Vec<ModelAlias>>,
    /// Settings for OAuth credentials' models, by channel (`oauth-settings`).
    pub oauth_settings: BTreeMap<String, Vec<ModelSetting>>,
    /// The `claude-api-key` entries.
    pub claude_keys: Vec<ApiKeyEntry>,
    /// The `codex-api-key` entries.
    pub codex_keys: Vec<ApiKeyEntry>,
}

/// Another name for a model (upstream's `OAuthModelAlias`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelAlias {
    /// The model's own name.
    pub name: String,
    /// The name to list it under.
    pub alias: String,
    /// List the model under both names, rather than only the alias.
    pub fork: bool,
    /// A display name for the alias, or empty to keep the model's.
    pub display_name: String,
}

/// Settings for a model (upstream's `OAuthModelSetting`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelSetting {
    /// The model, by name.
    pub name: String,
    /// The model, by an alias it is listed under, or empty.
    pub alias: String,
    /// The context window to list, or 0 to keep the model's.
    pub max_context_length: u64,
}

/// A `claude-api-key` or `codex-api-key` entry, as far as models go.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ApiKeyEntry {
    /// The API key.
    pub api_key: String,
    /// The base URL, or empty for the provider's.
    pub base_url: String,
    /// The models the key serves, or none for the catalog's.
    pub models: Vec<ConfiguredModel>,
    /// Models the key doesn't serve. `*` matches any text.
    pub excluded_models: Vec<String>,
}

impl fmt::Debug for ApiKeyEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKeyEntry")
            .field("base_url", &self.base_url)
            .field("models", &self.models)
            .field("excluded_models", &self.excluded_models)
            .finish_non_exhaustive()
    }
}

/// A model in an API key entry's list (upstream's `ClaudeModel` and
/// `CodexModel`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfiguredModel {
    /// The provider's name for the model.
    pub name: String,
    /// The name to list it under, or empty for `name`.
    pub alias: String,
    /// A display name, or empty.
    pub display_name: String,
    /// The context window to list, or 0 for none.
    pub max_context_length: u64,
    /// Turn on compatibility handling for the model.
    pub is_compat: bool,
    /// Its thinking settings, or `None` for the catalog's.
    pub thinking: Option<ThinkingSupport>,
    /// The model takes Codex's `configuration_update`. Codex only.
    pub support_configuration_update: bool,
}

/// What to do with a credential's registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthModels {
    /// Nothing: the credential has no ID.
    Ignore,
    /// Remove it: it is disabled, or serves no models.
    Unregister,
    /// Register these models for it under this provider.
    Register {
        /// The provider, lowercase.
        provider: String,
        /// The models, each ID trimmed.
        models: Vec<ModelInfo>,
    },
}

impl ModelRegistry {
    /// Registers the models `auth` serves under `rules`, or unregisters it if
    /// it is disabled or serves none (upstream's `registerModelsForAuth`).
    pub fn register_auth(&self, auth: &Auth, rules: &RegistrationRules) {
        match auth_models(auth, rules) {
            AuthModels::Ignore => {}
            AuthModels::Unregister => self.unregister_client(&auth.id),
            AuthModels::Register { provider, models } => {
                self.register_client(&auth.id, &provider, &models);
            }
        }
    }
}

/// The models `auth` serves under `rules`, from the built-in catalog.
pub fn auth_models(auth: &Auth, rules: &RegistrationRules) -> AuthModels {
    auth_models_with(auth, rules, StaticCatalog::embedded())
}

/// The models `auth` serves under `rules`, from `catalog`.
pub fn auth_models_with(
    auth: &Auth,
    rules: &RegistrationRules,
    catalog: &StaticCatalog,
) -> AuthModels {
    if auth.id.is_empty() {
        return AuthModels::Ignore;
    }
    if auth.disabled {
        return AuthModels::Unregister;
    }
    let kind = auth_kind(auth);
    let provider = go::to_lower(auth.provider.trim());
    let mut excluded = oauth_excluded_models(rules, &provider, kind);
    if let Some(list) = credential_excluded_models(auth, rules, &provider) {
        excluded = list;
    }

    let models = match provider.as_str() {
        "claude" => {
            let mut models = catalog.claude_models();
            if let Some(entry) = resolve_config_claude_key(auth, &rules.claude_keys) {
                if !entry.models.is_empty() {
                    models = build_config_models(&entry.models, "anthropic", "claude");
                }
                if kind == AUTH_KIND_API_KEY {
                    excluded.clone_from(&entry.excluded_models);
                }
            }
            apply_excluded_models(models, &excluded)
        }
        "codex" if kind == AUTH_KIND_API_KEY => {
            let mut models = Vec::new();
            if let Some(entry) = resolve_config_codex_key(auth, &rules.codex_keys) {
                models = build_codex_config_models(entry, catalog);
                excluded.clone_from(&entry.excluded_models);
            }
            apply_excluded_models(models, &excluded)
        }
        "codex" => {
            let plan = CodexPlan::from_plan_type(&codex_plan_type(auth));
            apply_excluded_models(catalog.codex_models(plan), &excluded)
        }
        _ => Vec::new(),
    };

    let models = apply_model_aliases(rules, &provider, kind, auth, models);
    if models.is_empty() {
        return AuthModels::Unregister;
    }
    let models = apply_model_settings(rules, &provider, kind, models);
    let models = apply_model_prefixes(models, &auth.prefix, rules.force_model_prefix);

    if provider.is_empty() {
        return AuthModels::Unregister;
    }
    let models: Vec<ModelInfo> = models
        .into_iter()
        .filter_map(|mut model| {
            let id = model.id.trim();
            if id.is_empty() {
                return None;
            }
            model.id = id.to_owned();
            Some(model)
        })
        .collect();
    if models.is_empty() {
        return AuthModels::Unregister;
    }
    AuthModels::Register { provider, models }
}

/// The credential's kind, [`AUTH_KIND_API_KEY`] or [`AUTH_KIND_OAUTH`], or
/// empty when it can't be told (upstream's `Auth.AuthKind`). An `auth_kind`
/// attribute or metadata value decides; failing that, an `api_key` attribute
/// makes an API key, and OAuth token fields an OAuth account.
pub fn auth_kind(auth: &Auth) -> &'static str {
    let kind = normalize_auth_kind(attribute(auth, "auth_kind"));
    if !kind.is_empty() {
        return kind;
    }
    let kind = normalize_auth_kind(metadata_string(auth, "auth_kind"));
    if !kind.is_empty() {
        return kind;
    }
    if !attribute(auth, "api_key").is_empty() {
        return AUTH_KIND_API_KEY;
    }
    if has_oauth_metadata(auth) {
        return AUTH_KIND_OAUTH;
    }
    ""
}

/// Where the credential came from, such as [`AUTH_SOURCE_CONFIG`], or empty
/// when it can't be told (upstream's `Auth.AuthSourceKind`).
pub fn auth_source_kind(auth: &Auth) -> &'static str {
    if equal_fold(attribute(auth, "runtime_only"), "true") {
        return AUTH_SOURCE_MEMORY;
    }
    let backend = normalize_auth_source_kind(attribute(auth, "source_backend"));
    if !backend.is_empty() {
        return backend;
    }
    let source = attribute(auth, "source");
    if !source.is_empty() {
        if go::to_lower(source).starts_with("config:") {
            return AUTH_SOURCE_CONFIG;
        }
        let source = normalize_auth_source_kind(source);
        return if source.is_empty() {
            AUTH_SOURCE_FILE
        } else {
            source
        };
    }
    if !attribute(auth, "path").is_empty() || !auth.file_name.trim().is_empty() {
        return AUTH_SOURCE_FILE;
    }
    ""
}

fn normalize_auth_kind(kind: &str) -> &'static str {
    match go::to_lower(kind.trim()).as_str() {
        "apikey" | "api_key" | "api-key" => AUTH_KIND_API_KEY,
        "oauth" | "oauth2" => AUTH_KIND_OAUTH,
        _ => "",
    }
}

fn normalize_auth_source_kind(source: &str) -> &'static str {
    match go::to_lower(source.trim()).as_str() {
        "config" => AUTH_SOURCE_CONFIG,
        "file" | "filesystem" => AUTH_SOURCE_FILE,
        "git" => AUTH_SOURCE_GIT,
        "memory" | "runtime" | "runtime_only" => AUTH_SOURCE_MEMORY,
        "objectstore" | "object-store" => AUTH_SOURCE_OBJECT_STORE,
        "postgres" | "postgresql" | "database" | "db" => AUTH_SOURCE_POSTGRES,
        _ => "",
    }
}

fn has_oauth_metadata(auth: &Auth) -> bool {
    if auth.metadata.is_empty() {
        return false;
    }
    let token_fields = [
        "access_token",
        "refresh_token",
        "id_token",
        "email",
        "token_type",
        "expires_at",
        "expired",
    ];
    if token_fields
        .iter()
        .any(|key| !metadata_string(auth, key).is_empty())
    {
        return true;
    }
    matches!(auth.metadata.get("token"), Some(Value::Object(token)) if !token.is_empty())
}

/// The attribute at `key`, trimmed, or empty.
fn attribute<'a>(auth: &'a Auth, key: &str) -> &'a str {
    auth.attribute(key).map_or("", str::trim)
}

/// The metadata string at `key`, trimmed, or empty.
fn metadata_string<'a>(auth: &'a Auth, key: &str) -> &'a str {
    auth.metadata_str(key).map_or("", str::trim)
}

/// The entries for `channel`, also under a key that differs only in case or
/// surrounding whitespace.
fn channel_entries<'a, T>(map: &'a BTreeMap<String, Vec<T>>, channel: &str) -> &'a [T] {
    map.get(channel)
        .or_else(|| {
            map.iter()
                .find(|(key, _)| go::to_lower(key.trim()) == channel)
                .map(|(_, entries)| entries)
        })
        .map_or(&[], Vec::as_slice)
}

/// The global exclusions for OAuth credentials of `provider` (upstream's
/// `oauthExcludedModels`).
fn oauth_excluded_models(rules: &RegistrationRules, provider: &str, kind: &str) -> Vec<String> {
    if go::to_lower(kind.trim()) == AUTH_KIND_API_KEY {
        return Vec::new();
    }
    channel_entries(&rules.oauth_excluded_models, &go::to_lower(provider.trim())).to_vec()
}

/// The credential's own complete exclusion list, if it has one: its
/// `excluded_models` attribute, a comma-separated list; or, without one, the
/// `excluded_models` list in its metadata merged with the global list.
fn credential_excluded_models(
    auth: &Auth,
    rules: &RegistrationRules,
    provider: &str,
) -> Option<Vec<String>> {
    if let Some(value) = auth.attributes.get("excluded_models") {
        if value.trim().is_empty() {
            return None;
        }
        return Some(value.split(',').map(str::to_owned).collect());
    }
    let raw = auth
        .metadata
        .get("excluded_models")
        .or_else(|| auth.metadata.get("excluded-models"));
    let Some(Value::Array(items)) = raw else {
        return None;
    };
    let mut excluded: Vec<String> = items
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect();
    if excluded.is_empty() {
        return None;
    }
    excluded.extend_from_slice(channel_entries(&rules.oauth_excluded_models, provider));
    Some(excluded)
}

/// The entry `entries[i]` for a credential from the config with
/// `config_index` i (upstream's `configEntryForAuthIndex`).
fn config_entry_for_auth_index<'a, T>(auth: &Auth, entries: &'a [T]) -> Option<&'a T> {
    if auth_source_kind(auth) != AUTH_SOURCE_CONFIG {
        return None;
    }
    let index: i64 = attribute(auth, "config_index").parse().ok()?;
    entries.get(usize::try_from(index).ok()?)
}

/// The `claude-api-key` entry for `auth`: by config index, else by key and
/// base URL, else by key (upstream's `resolveConfigClaudeKey`).
fn resolve_config_claude_key<'a>(
    auth: &Auth,
    entries: &'a [ApiKeyEntry],
) -> Option<&'a ApiKeyEntry> {
    if let Some(entry) = config_entry_for_auth_index(auth, entries) {
        return Some(entry);
    }
    let key = attribute(auth, "api_key");
    let base = attribute(auth, "base_url");
    for entry in entries {
        let entry_key = entry.api_key.trim();
        let entry_base = entry.base_url.trim();
        if !key.is_empty() && !base.is_empty() {
            if equal_fold(entry_key, key) && equal_fold(entry_base, base) {
                return Some(entry);
            }
            continue;
        }
        if !key.is_empty()
            && equal_fold(entry_key, key)
            && (entry_base.is_empty() || equal_fold(entry_base, base))
        {
            return Some(entry);
        }
        if key.is_empty() && !base.is_empty() && equal_fold(entry_base, base) {
            return Some(entry);
        }
    }
    if key.is_empty() {
        return None;
    }
    entries
        .iter()
        .find(|entry| equal_fold(entry.api_key.trim(), key))
}

/// The `codex-api-key` entry for `auth`: by config index if its key and base
/// URL match, else the first that matches (upstream's
/// `resolveConfigCodexStyleKey` as `resolveConfigCodexKey` calls it).
fn resolve_config_codex_key<'a>(
    auth: &Auth,
    entries: &'a [ApiKeyEntry],
) -> Option<&'a ApiKeyEntry> {
    let key = attribute(auth, "api_key");
    let base = attribute(auth, "base_url");
    let matches = |entry: &ApiKeyEntry| {
        let entry_key = entry.api_key.trim();
        let entry_base = entry.base_url.trim();
        if key.is_empty() {
            !base.is_empty() && equal_fold(entry_base, base)
        } else {
            equal_fold(entry_key, key) && (entry_base.is_empty() || equal_fold(entry_base, base))
        }
    };
    if let Some(entry) = config_entry_for_auth_index(auth, entries)
        && matches(entry)
    {
        return Some(entry);
    }
    entries.iter().find(|entry| matches(entry))
}

/// A configured model's details: listed under its alias, or its name
/// (upstream's `buildConfiguredModelInfo`).
fn build_configured_model_info(
    model: &ConfiguredModel,
    owned_by: &str,
    model_type: &str,
    created: i64,
    fallback_display_name: &str,
    user_defined: bool,
) -> Option<ModelInfo> {
    let name = model.name.trim();
    let alias = match model.alias.trim() {
        "" => name,
        alias => alias,
    };
    if alias.is_empty() {
        return None;
    }
    let display_name = [model.display_name.trim(), fallback_display_name, alias]
        .into_iter()
        .find(|candidate| !candidate.is_empty())
        .unwrap_or(alias);
    let metadata_model_id = if name.is_empty() { alias } else { name };
    let mut info = ModelInfo {
        id: alias.to_owned(),
        metadata_model_id: metadata_model_id.to_owned(),
        object: "model".to_owned(),
        created,
        owned_by: owned_by.to_owned(),
        model_type: model_type.to_owned(),
        display_name: display_name.to_owned(),
        user_defined,
        is_compat: model.is_compat,
        ..ModelInfo::default()
    };
    if model.max_context_length > 0 {
        info.context_length = model.max_context_length;
        info.max_context_length = model.max_context_length;
    }
    Some(info)
}

/// A config entry's models, each alias once, with thinking settings from the
/// config or the catalog (upstream's `buildConfigModels`).
fn build_config_models(
    models: &[ConfiguredModel],
    owned_by: &str,
    model_type: &str,
) -> Vec<ModelInfo> {
    if models.is_empty() {
        return Vec::new();
    }
    let now = chrono::Utc::now().timestamp();
    let mut out = Vec::with_capacity(models.len());
    let mut seen = HashSet::with_capacity(models.len());
    for model in models {
        let name = model.name.trim();
        let Some(mut info) =
            build_configured_model_info(model, owned_by, model_type, now, name, true)
        else {
            continue;
        };
        if !seen.insert(go::to_lower(&info.id)) {
            continue;
        }
        if model.thinking.is_some() {
            info.explicit_thinking = true;
        }
        if let Some(thinking) = resolve_thinking(name, model.thinking.as_ref()) {
            info.thinking = Some(thinking);
        }
        out.push(info);
    }
    out
}

/// A configured model's thinking settings: the config's, cleaned, or else the
/// catalog's for its name without a thinking suffix such as `(high)`
/// (upstream's `modelconfig.ResolveModelInfo`).
fn resolve_thinking(name: &str, configured: Option<&ThinkingSupport>) -> Option<ThinkingSupport> {
    if let Some(configured) = configured {
        return Some(normalize_thinking(configured));
    }
    let base = parse_suffix(name.trim()).trim();
    open_ferry_translate::models::ModelCatalog::embedded()
        .thinking(base)
        .cloned()
}

/// `model` without a trailing thinking suffix such as `(high)` (upstream's
/// `thinking.ParseSuffix`).
fn parse_suffix(model: &str) -> &str {
    match model.rfind('(') {
        Some(open) if model.ends_with(')') => model.get(..open).unwrap_or(model),
        _ => model,
    }
}

/// Configured thinking settings with levels trimmed, lowercase and each once.
/// A `none` level allows a zero budget, and `auto` a dynamic one (upstream's
/// `NormalizeThinkingSupport`).
fn normalize_thinking(raw: &ThinkingSupport) -> ThinkingSupport {
    let mut normalized = ThinkingSupport {
        levels: Vec::with_capacity(raw.levels.len()),
        ..raw.clone()
    };
    for level in &raw.levels {
        let level = go::to_lower(level.trim());
        match level.as_str() {
            "" => continue,
            "none" => normalized.zero_allowed = true,
            "auto" => normalized.dynamic_allowed = true,
            _ => {}
        }
        if !normalized.levels.contains(&level) {
            normalized.levels.push(level);
        }
    }
    normalized
}

/// A `codex-api-key` entry's models: its own list, with display names and
/// `configuration_update` support as configured, or else the Pro models
/// without `configuration_update` (upstream's `buildCodexConfigModels`).
fn build_codex_config_models(entry: &ApiKeyEntry, catalog: &StaticCatalog) -> Vec<ModelInfo> {
    if entry.models.is_empty() {
        let mut models = catalog.codex_models(CodexPlan::Pro);
        for model in &mut models {
            model.support_configuration_update = false;
        }
        return models;
    }

    let mut models = build_config_models(&entry.models, "openai", "openai");
    let mut display_names: HashMap<String, &str> = HashMap::with_capacity(entry.models.len());
    let mut configuration_updates: HashMap<String, bool> =
        HashMap::with_capacity(entry.models.len());
    for model in &entry.models {
        let alias = match model.alias.trim() {
            "" => model.name.trim(),
            alias => alias,
        };
        if alias.is_empty() {
            continue;
        }
        let key = go::to_lower(alias);
        if configuration_updates.contains_key(&key) {
            continue;
        }
        configuration_updates.insert(key.clone(), model.support_configuration_update);
        let display_name = model.display_name.trim();
        if !display_name.is_empty() {
            display_names.insert(key, display_name);
        }
    }
    for model in &mut models {
        let key = go::to_lower(&model.id);
        if let Some(display_name) = display_names.get(&key) {
            (*display_name).clone_into(&mut model.display_name);
        }
        model.support_configuration_update =
            configuration_updates.get(&key).copied().unwrap_or(false);
    }
    models
}

/// `models` without those an exclusion pattern matches, ignoring case
/// (upstream's `applyExcludedModels`).
fn apply_excluded_models(models: Vec<ModelInfo>, excluded: &[String]) -> Vec<ModelInfo> {
    if models.is_empty() || excluded.is_empty() {
        return models;
    }
    let patterns: Vec<String> = excluded
        .iter()
        .map(|pattern| pattern.trim())
        .filter(|pattern| !pattern.is_empty())
        .map(go::to_lower)
        .collect();
    if patterns.is_empty() {
        return models;
    }
    models
        .into_iter()
        .filter(|model| {
            let id = go::to_lower(model.id.trim());
            !patterns.iter().any(|pattern| match_wildcard(pattern, &id))
        })
        .collect()
}

/// Whether `value` matches `pattern`, where `*` matches any text (upstream's
/// `matchWildcard`).
fn match_wildcard(pattern: &str, value: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    if !pattern.contains('*') {
        return pattern == value;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut value = value;
    if let Some(prefix) = parts.first().filter(|prefix| !prefix.is_empty()) {
        let Some(rest) = value.strip_prefix(prefix) else {
            return false;
        };
        value = rest;
    }
    if let Some(suffix) = parts.last().filter(|suffix| !suffix.is_empty()) {
        let Some(rest) = value.strip_suffix(suffix) else {
            return false;
        };
        value = rest;
    }
    let middle = parts.len().saturating_sub(2);
    for segment in parts.iter().skip(1).take(middle) {
        if segment.is_empty() {
            continue;
        }
        let Some((_, rest)) = value.split_once(segment) else {
            return false;
        };
        value = rest;
    }
    true
}

/// Each model also under `prefix/`, and with `force` only so, unless its ID
/// is the prefix itself (upstream's `applyModelPrefixes`).
fn apply_model_prefixes(models: Vec<ModelInfo>, prefix: &str, force: bool) -> Vec<ModelInfo> {
    let prefix = prefix.trim();
    if prefix.is_empty() || models.is_empty() {
        return models;
    }
    let mut out = Vec::with_capacity(models.len() * 2);
    let mut seen = HashSet::with_capacity(models.len() * 2);
    let mut add = |model: ModelInfo| {
        let id = model.id.trim();
        if !id.is_empty() && seen.insert(id.to_owned()) {
            out.push(model);
        }
    };
    for model in models {
        let base = model.id.trim().to_owned();
        if base.is_empty() {
            continue;
        }
        let mut prefixed = model.clone();
        prefixed.id = format!("{prefix}/{base}");
        if prefixed.metadata_model_id.is_empty() {
            prefixed.metadata_model_id.clone_from(&base);
        }
        if !force || prefix == base {
            add(model);
        }
        add(prefixed);
    }
    out
}

/// The channel that alias and settings rules are keyed by: the provider, or
/// none for API keys and Gemini (upstream's `OAuthModelAliasChannel`).
fn alias_channel(provider: &str, kind: &str) -> String {
    let kind = match go::to_lower(kind.trim()).as_str() {
        "api_key" | "api-key" => AUTH_KIND_API_KEY.to_owned(),
        kind => kind.to_owned(),
    };
    if kind == AUTH_KIND_API_KEY {
        return String::new();
    }
    match go::to_lower(provider.trim()).as_str() {
        "gemini" => String::new(),
        provider => provider.to_owned(),
    }
}

/// `models` with the aliases for the credential and its channel applied
/// (upstream's `applyOAuthModelAliasForAuth`).
fn apply_model_aliases(
    rules: &RegistrationRules,
    provider: &str,
    kind: &str,
    auth: &Auth,
    models: Vec<ModelInfo>,
) -> Vec<ModelInfo> {
    if models.is_empty() {
        return models;
    }
    let channel = alias_channel(provider, kind);
    if channel.is_empty() {
        return models;
    }
    let aliases = aliases_for_auth(rules, &channel, per_auth_aliases(auth));
    if aliases.is_empty() {
        return models;
    }
    apply_alias_entries(&aliases, models)
}

/// The credential's own aliases followed by the channel's, each alias once
/// (upstream's `oauthModelAliasesForAuth`).
fn aliases_for_auth(
    rules: &RegistrationRules,
    channel: &str,
    per_auth: Vec<ModelAlias>,
) -> Vec<ModelAlias> {
    if rules.oauth_model_alias.is_empty() {
        return per_auth;
    }
    let global = channel_entries(&rules.oauth_model_alias, channel);
    if per_auth.is_empty() {
        return global.to_vec();
    }
    if global.is_empty() {
        return per_auth;
    }
    let mut out = Vec::with_capacity(per_auth.len() + global.len());
    let mut seen = HashSet::with_capacity(per_auth.len() + global.len());
    for entry in per_auth.iter().chain(global) {
        let alias = entry.alias.trim();
        if alias.is_empty() || !seen.insert(go::to_lower(alias)) {
            continue;
        }
        out.push(entry.clone());
    }
    out
}

/// The credential's own aliases: its `model_aliases` attribute, a JSON list,
/// or without one, the `model_aliases` list in its metadata (upstream's
/// `OAuthModelAliasesFromAttributes`). A list that doesn't decode counts as
/// none.
fn per_auth_aliases(auth: &Auth) -> Vec<ModelAlias> {
    let parsed = match auth.attributes.get("model_aliases") {
        Some(raw) => {
            let raw = raw.trim();
            if raw.is_empty() {
                return Vec::new();
            }
            serde_json::from_str::<Value>(raw)
                .ok()
                .and_then(|value| parse_aliases(&value))
        }
        None => match auth
            .metadata
            .get("model_aliases")
            .or_else(|| auth.metadata.get("model-aliases"))
        {
            None | Some(Value::Null) => None,
            Some(value) => parse_aliases(value),
        },
    };
    sanitize_aliases(parsed.unwrap_or_default())
}

/// Decodes a JSON list of aliases as Go decodes it into
/// `[]OAuthModelAlias`, or `None` if it doesn't decode.
fn parse_aliases(value: &Value) -> Option<Vec<ModelAlias>> {
    const FIELDS: [&str; 5] = ["name", "alias", "fork", "display-name", "force-mapping"];
    let Some(items) = json::array("aliases", value).ok()? else {
        return Some(Vec::new());
    };
    let mut aliases = Vec::with_capacity(items.len());
    for item in items {
        let mut alias = ModelAlias::default();
        if let Some(object) = json::object("alias", item).ok()? {
            for (key, value) in object {
                match json::field(key, &FIELDS) {
                    Some("name") => json::string(key, value, &mut alias.name).ok()?,
                    Some("alias") => json::string(key, value, &mut alias.alias).ok()?,
                    Some("fork") => json::boolean(key, value, &mut alias.fork).ok()?,
                    Some("display-name") => {
                        json::string(key, value, &mut alias.display_name).ok()?;
                    }
                    Some("force-mapping") => json::boolean(key, value, &mut false).ok()?,
                    _ => {}
                }
            }
        }
        aliases.push(alias);
    }
    Some(aliases)
}

/// Aliases trimmed, without those missing a name or alias, those naming the
/// model itself, or repeats of an alias (upstream's
/// `SanitizeOAuthModelAlias`).
fn sanitize_aliases(aliases: Vec<ModelAlias>) -> Vec<ModelAlias> {
    let mut seen = HashSet::with_capacity(aliases.len());
    let mut clean = Vec::with_capacity(aliases.len());
    for entry in aliases {
        let name = entry.name.trim();
        let alias = entry.alias.trim();
        if name.is_empty() || alias.is_empty() || equal_fold(name, alias) {
            continue;
        }
        if !seen.insert(go::to_lower(alias)) {
            continue;
        }
        clean.push(ModelAlias {
            name: name.to_owned(),
            alias: alias.to_owned(),
            fork: entry.fork,
            display_name: entry.display_name.trim().to_owned(),
        });
    }
    clean
}

/// `models` renamed or, with `fork`, added to under their aliases (upstream's
/// `applyOAuthModelAliasEntries`). An alias keeps the model's metadata ID.
fn apply_alias_entries(aliases: &[ModelAlias], models: Vec<ModelInfo>) -> Vec<ModelInfo> {
    struct Entry<'a> {
        alias: &'a str,
        display_name: &'a str,
        fork: bool,
    }
    let mut forward: HashMap<String, Vec<Entry<'_>>> = HashMap::with_capacity(aliases.len());
    for entry in aliases {
        let name = entry.name.trim();
        let alias = entry.alias.trim();
        if name.is_empty() || alias.is_empty() || equal_fold(name, alias) {
            continue;
        }
        forward.entry(go::to_lower(name)).or_default().push(Entry {
            alias,
            display_name: entry.display_name.trim(),
            fork: entry.fork,
        });
    }
    if forward.is_empty() {
        return models;
    }

    let mut out = Vec::with_capacity(models.len());
    let mut seen: HashSet<String> = HashSet::with_capacity(models.len());
    for model in models {
        let id = model.id.trim().to_owned();
        if id.is_empty() {
            continue;
        }
        let key = go::to_lower(&id);
        let entries = forward.get(&key).map_or(&[][..], Vec::as_slice);
        if entries.is_empty() {
            if seen.insert(key) {
                out.push(model);
            }
            continue;
        }

        let keep_original = entries.iter().any(|entry| entry.fork);
        if keep_original && seen.insert(key.clone()) {
            out.push(model.clone());
        }
        let mut added_alias = false;
        for entry in entries {
            let mapped = entry.alias.trim();
            if mapped.is_empty() || equal_fold(mapped, &id) || !seen.insert(go::to_lower(mapped)) {
                continue;
            }
            let mut clone = model.clone();
            clone.id = mapped.to_owned();
            if clone.metadata_model_id.is_empty() {
                clone.metadata_model_id.clone_from(&id);
            }
            if !entry.display_name.is_empty() {
                entry.display_name.clone_into(&mut clone.display_name);
            }
            if !clone.name.is_empty() {
                clone.name = rewrite_model_info_name(&clone.name, &id, mapped);
            }
            out.push(clone);
            added_alias = true;
        }
        if !keep_original && !added_alias && seen.insert(key) {
            out.push(model);
        }
    }
    out
}

/// A Gemini-style model name with `old_id` swapped for `new_id`, as in
/// `models/gemini-2.5-pro` (upstream's `rewriteModelInfoName`).
fn rewrite_model_info_name(name: &str, old_id: &str, new_id: &str) -> String {
    let trimmed = name.trim();
    let (old_id, new_id) = (old_id.trim(), new_id.trim());
    if trimmed.is_empty() || old_id.is_empty() || new_id.is_empty() || equal_fold(old_id, new_id) {
        return name.to_owned();
    }
    if equal_fold(trimmed, old_id) {
        return new_id.to_owned();
    }
    if trimmed.ends_with(&format!("/{old_id}"))
        && let Some(prefix) = trimmed.strip_suffix(old_id)
    {
        return format!("{prefix}{new_id}");
    }
    if trimmed == format!("models/{old_id}") {
        return format!("models/{new_id}");
    }
    name.to_owned()
}

/// `models` with the channel's settings applied (upstream's
/// `applyOAuthSettingsForAuth`).
fn apply_model_settings(
    rules: &RegistrationRules,
    provider: &str,
    kind: &str,
    mut models: Vec<ModelInfo>,
) -> Vec<ModelInfo> {
    if models.is_empty() {
        return models;
    }
    let channel = alias_channel(provider, kind);
    if channel.is_empty() {
        return models;
    }
    let settings = channel_entries(&rules.oauth_settings, &channel);
    if settings.is_empty() {
        return models;
    }
    for model in &mut models {
        if let Some(setting) =
            resolve_model_setting(settings, &model.id, &model.metadata_model_id, &model.name)
            && setting.max_context_length > 0
        {
            model.context_length = setting.max_context_length;
            model.max_context_length = setting.max_context_length;
        }
    }
    models
}

/// The setting for a model: the last whose alias is the model's ID, or else
/// the last whose name is its ID, metadata ID or Gemini-style name, ignoring
/// case (upstream's `ResolveOAuthModelSetting`).
fn resolve_model_setting<'a>(
    settings: &'a [ModelSetting],
    model_id: &str,
    metadata_model_id: &str,
    model_name: &str,
) -> Option<&'a ModelSetting> {
    let id = go::to_lower(model_id.trim());
    let metadata_id = go::to_lower(metadata_model_id.trim());
    let name = go::to_lower(model_name.trim());
    let mut alias_match = None;
    let mut name_match = None;
    for setting in settings {
        let entry_name = go::to_lower(setting.name.trim());
        if entry_name.is_empty() {
            continue;
        }
        let entry_alias = go::to_lower(setting.alias.trim());
        if !entry_alias.is_empty() && !id.is_empty() && id == entry_alias {
            alias_match = Some(setting);
        } else if (entry_alias.is_empty() || entry_alias == id)
            && (id == entry_name
                || (!metadata_id.is_empty() && metadata_id == entry_name)
                || (!name.is_empty() && name == entry_name))
        {
            name_match = Some(setting);
        }
    }
    alias_match.or(name_match)
}

/// A Codex account's ChatGPT plan type: its `plan_type` attribute, or without
/// one, the `plan_type` in its metadata, or else the plan claim of its
/// `id_token`, or `free` if that can't be read. Empty when there is none.
fn codex_plan_type(auth: &Auth) -> String {
    if let Some(plan_type) = auth.attributes.get("plan_type") {
        return plan_type.trim().to_owned();
    }
    let plan_type = metadata_string(auth, "plan_type");
    if !plan_type.is_empty() {
        return plan_type.to_owned();
    }
    match auth.metadata_str("id_token") {
        Some(token) if !token.trim().is_empty() => jwt_plan_type(token),
        _ => String::new(),
    }
}

/// The ChatGPT plan type in an OpenAI ID token's claims, or `free` if the
/// token can't be read or has none (upstream's `ParseJWTToken` and
/// `GetPlanType`). The signature isn't checked: the plan type only picks
/// which models to list.
fn jwt_plan_type(token: &str) -> String {
    claims_plan_type(token)
        .map(|plan_type| plan_type.trim().to_owned())
        .filter(|plan_type| !plan_type.is_empty())
        .unwrap_or_else(|| DEFAULT_CODEX_PLAN_TYPE.to_owned())
}

fn claims_plan_type(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    let [_, claims, _] = parts.as_slice() else {
        return None;
    };
    // Go pads by the raw length, then skips line breaks as it decodes.
    let mut padded = (*claims).to_owned();
    match claims.len() % 4 {
        2 => padded.push_str("=="),
        3 => padded.push('='),
        _ => {}
    }
    padded.retain(|c| c != '\r' && c != '\n');
    let bytes = JWT_BASE64.decode(padded).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let claims = json::object("claims", &value).ok()??;
    let mut plan_type = String::new();
    for (key, value) in claims {
        if json::field(key, &[OPENAI_AUTH_CLAIM]).is_none() {
            continue;
        }
        let Some(info) = json::object(key, value).ok()? else {
            continue;
        };
        for (key, value) in info {
            if json::field(key, &["chatgpt_plan_type"]).is_some() {
                json::string(key, value, &mut plan_type).ok()?;
            }
        }
    }
    Some(plan_type)
}
