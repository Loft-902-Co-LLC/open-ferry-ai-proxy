// Ported from CLIProxyAPI internal/config (config.go, sdk_config.go,
// config_types.go and vertex_compat.go: the JSON layouts of `Config` and the
// types it holds), internal/registry/catalog_config.go (CatalogSources) and
// internal/api/handlers/management/config_auth_index.go
// (the `*WithAuthIndex` types) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The config as Go's JSON encoder writes upstream's `Config`: struct
//! fields in their declaration order, `omitempty` fields left out when
//! empty, and a slice without `omitempty` written as `null` when it is
//! empty.
//!
//! Deviations from upstream:
//! - Only the sections open-ferry types are written; see
//!   [`config`](super) for the ones left out.
//! - A list the file gives as `[]` is written as `null`, as it is when the
//!   file leaves it out: the typed config doesn't tell the two apart, where
//!   upstream writes `[]` for the first. The same goes for a payload rule's
//!   `params`.
//! - A payload value Go's JSON encoder refuses (a mapping with a key that
//!   isn't a string, a time in a zone a day or more from UTC, or an
//!   infinite or NaN float) is written as `null`; upstream's whole answer
//!   fails.

use std::collections::BTreeMap;

use open_ferry_core::config::{
    AnyValue, ClaudeKey, ClaudeModel, CodexKey, CodexModel, Config, DisableImageGeneration,
    GeminiKey, GeminiModel, OAuthModelAlias, OAuthModelSetting, OpenAiCompatibility,
    OpenAiCompatibilityApiKey, OpenAiCompatibilityModel, PayloadConfig, PayloadFilterRule,
    PayloadModelRule, PayloadRule, RequestScopedErrorRule, ThinkingSupport, VertexCompatKey,
    VertexCompatModel,
};
use serde_json::Value;

use crate::json::Json;

/// A Go struct's fields, in declaration order.
pub(crate) struct Fields(Vec<(&'static str, Json)>);

impl Fields {
    pub(crate) fn new() -> Self {
        Self(Vec::new())
    }

    /// A field written whatever its value.
    pub(crate) fn with(mut self, name: &'static str, value: Json) -> Self {
        self.0.push((name, value));
        self
    }

    /// A field tagged `omitempty`: left out when it is `false`, 0, `""`,
    /// `null` or an empty list or map.
    pub(crate) fn omit_empty(self, name: &'static str, value: Json) -> Self {
        if is_empty(&value) {
            self
        } else {
            self.with(name, value)
        }
    }

    /// A pointer field tagged `omitempty`: left out when it is nil.
    pub(crate) fn omit_nil(self, name: &'static str, value: Option<Json>) -> Self {
        match value {
            Some(value) => self.with(name, value),
            None => self,
        }
    }

    pub(crate) fn done(self) -> Json {
        Json::Struct(self.0)
    }
}

/// Whether `omitempty` leaves `value` out.
fn is_empty(value: &Json) -> bool {
    match value {
        Json::Null => true,
        Json::Bool(b) => !b,
        Json::Int(n) => *n == 0,
        Json::Uint(n) => *n == 0,
        Json::Str(s) => s.is_empty(),
        Json::Bytes(bytes) => bytes.is_empty(),
        Json::Array(items) => items.is_empty(),
        Json::Map(entries) => entries.is_empty(),
        Json::Time(_) | Json::Struct(_) | Json::Any(_) => false,
    }
}

fn string(value: &str) -> Json {
    Json::Str(value.to_owned())
}

/// A slice: `null` when empty, as Go writes a nil slice.
fn slice<T>(items: &[T], write: impl Fn(&T) -> Json) -> Json {
    if items.is_empty() {
        Json::Null
    } else {
        Json::Array(items.iter().map(write).collect())
    }
}

/// A `[]string`: `null` when empty.
pub(crate) fn strings(items: &[String]) -> Json {
    slice(items, |item| string(item))
}

/// A `map[string]string`: `null` when empty.
fn string_map(entries: &BTreeMap<String, String>) -> Json {
    if entries.is_empty() {
        return Json::Null;
    }
    Json::Map(
        entries
            .iter()
            .map(|(key, value)| (key.clone(), string(value)))
            .collect(),
    )
}

/// A `map[string][]T`: `null` when empty.
fn list_map<T>(entries: &BTreeMap<String, Vec<T>>, write: impl Fn(&T) -> Json) -> Json {
    if entries.is_empty() {
        return Json::Null;
    }
    Json::Map(
        entries
            .iter()
            .map(|(key, items)| (key.clone(), slice(items, &write)))
            .collect(),
    )
}

/// The config, as upstream's `GetConfig` writes it.
pub(super) fn config(config: &Config) -> Json {
    let codex_client = &config.client.codex;
    let streaming = &config.streaming;
    let tls = &config.tls;
    let quota = &config.quota_exceeded;
    let codex = &config.codex;
    let models = &config.models;
    Fields::new()
        // registry.CatalogSources, with every field omitempty.
        .with(
            "models",
            Fields::new()
                .omit_empty("catalog", string(&models.catalog))
                .omit_empty("codex-catalog", string(&models.codex_catalog))
                .omit_empty("devin-catalog", string(&models.devin_catalog))
                .done(),
        )
        .with(
            "client",
            Fields::new()
                .with(
                    "codex",
                    Fields::new()
                        .with(
                            "optimize-multi-agent-v2",
                            Json::Bool(codex_client.optimize_multi_agent_v2),
                        )
                        .with(
                            "enable-apply-patch",
                            Json::Bool(codex_client.enable_apply_patch),
                        )
                        .done(),
                )
                .done(),
        )
        .with("proxy-url", string(&config.proxy_url))
        .with(
            "disable-image-generation",
            image_generation(config.disable_image_generation),
        )
        .omit_empty(
            "gpt-image-2-base-model",
            string(&config.gpt_image_2_base_model),
        )
        .omit_empty(
            "video-result-auth-cache-ttl",
            string(&config.video_result_auth_cache_ttl),
        )
        .with("force-model-prefix", Json::Bool(config.force_model_prefix))
        .with("request-log", Json::Bool(config.request_log))
        .with("api-keys", strings(&config.api_keys))
        .with(
            "passthrough-headers",
            Json::Bool(config.passthrough_headers),
        )
        .with(
            "streaming",
            Fields::new()
                .omit_empty("keepalive-seconds", Json::Int(streaming.keepalive_seconds))
                .omit_empty("bootstrap-retries", Json::Int(streaming.bootstrap_retries))
                .done(),
        )
        .omit_empty(
            "nonstream-keepalive-interval",
            Json::Int(config.nonstream_keepalive_interval),
        )
        .with("trusted-proxies", strings(&config.trusted_proxies))
        .with(
            "tls",
            Fields::new()
                .with("enable", Json::Bool(tls.enable))
                .with("cert", string(&tls.cert))
                .with("key", string(&tls.key))
                .done(),
        )
        .with("debug", Json::Bool(config.debug))
        .with("commercial-mode", Json::Bool(config.commercial_mode))
        .with("logging-to-file", Json::Bool(config.logging_to_file))
        .with(
            "logs-max-total-size-mb",
            Json::Int(config.logs_max_total_size_mb),
        )
        .with(
            "error-logs-max-files",
            Json::Int(config.error_logs_max_files),
        )
        .with(
            "usage-statistics-enabled",
            Json::Bool(config.usage_statistics_enabled),
        )
        .with(
            "redis-usage-queue-retention-seconds",
            Json::Int(config.redis_usage_queue_retention_seconds),
        )
        .with("disable-cooling", Json::Bool(config.disable_cooling))
        .with(
            "save-cooldown-status",
            Json::Bool(config.save_cooldown_status),
        )
        .with(
            "transient-error-cooldown-seconds",
            Json::Int(config.transient_error_cooldown_seconds),
        )
        .with(
            "auth-auto-refresh-workers",
            Json::Int(config.auth_auto_refresh_workers),
        )
        .with("request-retry", Json::Int(config.request_retry))
        .with(
            "max-retry-credentials",
            Json::Int(config.max_retry_credentials),
        )
        .with("max-retry-interval", Json::Int(config.max_retry_interval))
        .with(
            "quota-exceeded",
            Fields::new()
                .with("switch-project", Json::Bool(quota.switch_project))
                .with(
                    "switch-preview-model",
                    Json::Bool(quota.switch_preview_model),
                )
                .with("antigravity-credits", Json::Bool(quota.antigravity_credits))
                .done(),
        )
        .with(
            "routing",
            Fields::new()
                .omit_empty("strategy", string(&config.routing.strategy))
                .omit_empty(
                    "session-affinity",
                    Json::Bool(config.routing.session_affinity),
                )
                .omit_empty(
                    "session-affinity-ttl",
                    string(&config.routing.session_affinity_ttl),
                )
                .omit_nil(
                    "session-affinity-subagents",
                    config.routing.session_affinity_subagents.map(Json::Bool),
                )
                .done(),
        )
        .with("ws-auth", Json::Bool(config.ws_auth))
        .with(
            "gemini-api-key",
            slice(&config.gemini_api_key, |key| gemini_key(key, "")),
        )
        .with(
            "interactions-api-key",
            slice(&config.interactions_api_key, |key| gemini_key(key, "")),
        )
        .with(
            "codex-api-key",
            slice(&config.codex_api_key, |key| codex_key(key, "")),
        )
        .with(
            "xai-api-key",
            slice(&config.xai_api_key, |key| codex_key(key, "")),
        )
        .with(
            "meta-api-key",
            slice(&config.meta_api_key, |key| codex_key(key, "")),
        )
        .with(
            "xai",
            Fields::new()
                .with("inject-x-search", Json::Bool(config.xai.inject_x_search))
                .done(),
        )
        .with(
            "codex",
            Fields::new()
                .with(
                    "stream-bootstrap-buffering",
                    Json::Bool(codex.stream_bootstrap_buffering),
                )
                .omit_empty(
                    "stream-bootstrap-timeout",
                    string(&codex.stream_bootstrap_timeout),
                )
                .with(
                    "orphan-delegation-compatibility",
                    Json::Bool(codex.orphan_delegation_compatibility),
                )
                .with("model-level-cooling", Json::Bool(codex.model_level_cooling))
                .with("response-steering", Json::Bool(codex.response_steering))
                .done(),
        )
        .with(
            "codex-header-defaults",
            Fields::new()
                .with(
                    "beta-features",
                    string(&config.codex_header_defaults.beta_features),
                )
                .done(),
        )
        .with(
            "claude",
            Fields::new()
                .with(
                    "model-level-cooling",
                    Json::Bool(config.claude.model_level_cooling),
                )
                .done(),
        )
        .with(
            "claude-api-key",
            slice(&config.claude_api_key, |key| claude_key(key, "")),
        )
        .with(
            "openai-compatibility",
            slice(&config.openai_compatibility, openai_compatibility),
        )
        .with(
            "vertex-api-key",
            slice(&config.vertex_api_key, |key| vertex_key(key, "")),
        )
        .omit_empty(
            "oauth-excluded-models",
            excluded_models(&config.oauth_excluded_models),
        )
        .omit_empty(
            "oauth-model-alias",
            model_aliases(&config.oauth_model_alias),
        )
        .omit_empty(
            "oauth-request-scoped-errors",
            scoped_errors(&config.oauth_request_scoped_errors),
        )
        .omit_empty(
            "oauth-settings",
            list_map(&config.oauth_settings, model_setting),
        )
        .with("payload", payload(&config.payload))
        .done()
}

/// `disable-image-generation`, as upstream's `MarshalJSON` writes it: a
/// switch for `false` and `true`, a text for `chat` and `passthrough`.
fn image_generation(mode: DisableImageGeneration) -> Json {
    match mode {
        DisableImageGeneration::Off => Json::Bool(false),
        DisableImageGeneration::All => Json::Bool(true),
        DisableImageGeneration::Chat | DisableImageGeneration::Passthrough => string(mode.as_str()),
    }
}

/// `payload`.
fn payload(payload: &PayloadConfig) -> Json {
    Fields::new()
        .with("default", slice(&payload.default, payload_rule))
        .with("default-raw", slice(&payload.default_raw, payload_rule))
        .with("override", slice(&payload.r#override, payload_rule))
        .with("override-raw", slice(&payload.override_raw, payload_rule))
        .with("filter", slice(&payload.filter, payload_filter_rule))
        .done()
}

fn payload_rule(rule: &PayloadRule) -> Json {
    let params = if rule.params.is_empty() {
        Json::Null
    } else {
        Json::Map(
            rule.params
                .iter()
                .map(|(path, value)| (path.clone(), any_value(value)))
                .collect(),
        )
    };
    Fields::new()
        .with("models", slice(&rule.models, payload_model_rule))
        .with("params", params)
        .done()
}

fn payload_filter_rule(rule: &PayloadFilterRule) -> Json {
    Fields::new()
        .with("models", slice(&rule.models, payload_model_rule))
        .with("params", strings(&rule.params))
        .done()
}

fn payload_model_rule(rule: &PayloadModelRule) -> Json {
    let conditions = |entries: &BTreeMap<String, AnyValue>| {
        Json::Map(
            entries
                .iter()
                .map(|(path, value)| (path.clone(), any_value(value)))
                .collect(),
        )
    };
    Fields::new()
        .with("name", string(&rule.name))
        .with("protocol", string(&rule.protocol))
        .with("headers", string_map(&rule.headers))
        .with("from-protocol", string(&rule.from_protocol))
        .with("match", slice(&rule.r#match, conditions))
        .with("not-match", slice(&rule.not_match, conditions))
        .with("exist", strings(&rule.exist))
        .with("not-exist", strings(&rule.not_exist))
        .done()
}

/// A value decoded from YAML into `any`. What Go's encoder refuses is
/// `null`.
fn any_value(value: &AnyValue) -> Json {
    match value {
        AnyValue::Null | AnyValue::Time(None, _) | AnyValue::AnyMap => Json::Null,
        AnyValue::Bool(b) => Json::Bool(*b),
        AnyValue::Int(n) => Json::Int(*n),
        AnyValue::Uint(n) => Json::Uint(*n),
        AnyValue::Float(f) => Json::Any(Value::from(*f)),
        AnyValue::Str(s) | AnyValue::Time(Some(s), _) => string(s),
        AnyValue::Seq(items) => Json::Array(items.iter().map(any_value).collect()),
        AnyValue::Map(entries) => Json::Map(
            entries
                .iter()
                .map(|(key, item)| (key.clone(), any_value(item)))
                .collect(),
        ),
    }
}

/// `oauth-excluded-models`.
pub(super) fn excluded_models(entries: &BTreeMap<String, Vec<String>>) -> Json {
    list_map(entries, |model| string(model))
}

/// `oauth-model-alias`.
pub(super) fn model_aliases(entries: &BTreeMap<String, Vec<OAuthModelAlias>>) -> Json {
    list_map(entries, model_alias)
}

/// `oauth-request-scoped-errors`.
pub(super) fn scoped_errors(entries: &BTreeMap<String, Vec<RequestScopedErrorRule>>) -> Json {
    list_map(entries, scoped_error)
}

/// A `gemini-api-key` or `interactions-api-key` entry, with its
/// credential's `auth-index` when that isn't empty (upstream's `GeminiKey`
/// and `geminiKeyWithAuthIndex`).
pub(super) fn gemini_key(key: &GeminiKey, auth_index: &str) -> Json {
    Fields::new()
        .with("api-key", string(&key.api_key))
        .omit_empty("priority", Json::Int(key.priority))
        .omit_nil("weight", key.weight.map(Json::Int))
        .omit_empty("prefix", string(&key.prefix))
        .omit_empty("base-url", string(&key.base_url))
        .omit_empty("proxy-url", string(&key.proxy_url))
        .omit_empty("models", slice(&key.models, gemini_model))
        .omit_empty("headers", string_map(&key.headers))
        .omit_empty("excluded-models", strings(&key.excluded_models))
        .omit_nil("disable-cooling", key.disable_cooling.map(Json::Bool))
        .omit_nil("request-retry", key.request_retry.map(Json::Int))
        .omit_empty(
            "request-scoped-errors",
            slice(&key.request_scoped_errors, scoped_error),
        )
        .omit_empty("auth-index", string(auth_index))
        .done()
}

/// A `claude-api-key` entry, with its credential's `auth-index` when that
/// isn't empty (upstream's `ClaudeKey` and `claudeKeyWithAuthIndex`).
pub(super) fn claude_key(key: &ClaudeKey, auth_index: &str) -> Json {
    Fields::new()
        .with("api-key", string(&key.api_key))
        .omit_empty("priority", Json::Int(key.priority))
        .omit_nil("weight", key.weight.map(Json::Int))
        .omit_empty("prefix", string(&key.prefix))
        .with("base-url", string(&key.base_url))
        .with("proxy-url", string(&key.proxy_url))
        .with("models", slice(&key.models, claude_model))
        .omit_empty("headers", string_map(&key.headers))
        .omit_empty("excluded-models", strings(&key.excluded_models))
        .omit_empty(
            "rebuild-mid-system-message",
            Json::Bool(key.rebuild_mid_system_message),
        )
        .omit_nil("disable-cooling", key.disable_cooling.map(Json::Bool))
        .omit_nil("request-retry", key.request_retry.map(Json::Int))
        .omit_empty(
            "request-scoped-errors",
            slice(&key.request_scoped_errors, scoped_error),
        )
        .omit_empty("auth-index", string(auth_index))
        .done()
}

/// A `codex-api-key`, `xai-api-key` or `meta-api-key` entry, with its
/// credential's `auth-index` when that isn't empty (upstream's `CodexKey`,
/// which `XAIKey` and `MetaKey` alias, and `codexKeyWithAuthIndex`).
pub(super) fn codex_key(key: &CodexKey, auth_index: &str) -> Json {
    Fields::new()
        .with("api-key", string(&key.api_key))
        .omit_empty("priority", Json::Int(key.priority))
        .omit_nil("weight", key.weight.map(Json::Int))
        .omit_empty("prefix", string(&key.prefix))
        .with("base-url", string(&key.base_url))
        .omit_empty("websockets", Json::Bool(key.websockets))
        .omit_empty("alpha-search", Json::Bool(key.alpha_search))
        .with("proxy-url", string(&key.proxy_url))
        .with("models", slice(&key.models, codex_model))
        .omit_empty("headers", string_map(&key.headers))
        .omit_empty("excluded-models", strings(&key.excluded_models))
        .omit_nil("disable-cooling", key.disable_cooling.map(Json::Bool))
        .omit_nil("request-retry", key.request_retry.map(Json::Int))
        .omit_empty(
            "request-scoped-errors",
            slice(&key.request_scoped_errors, scoped_error),
        )
        .omit_empty("auth-index", string(auth_index))
        .done()
}

/// A `vertex-api-key` entry, with its credential's `auth-index` when that
/// isn't empty (upstream's `VertexCompatKey` and
/// `vertexCompatKeyWithAuthIndex`).
pub(super) fn vertex_key(key: &VertexCompatKey, auth_index: &str) -> Json {
    Fields::new()
        .with("api-key", string(&key.api_key))
        .omit_empty("priority", Json::Int(key.priority))
        .omit_nil("weight", key.weight.map(Json::Int))
        .omit_empty("prefix", string(&key.prefix))
        .omit_empty("base-url", string(&key.base_url))
        .omit_empty("proxy-url", string(&key.proxy_url))
        .omit_empty("headers", string_map(&key.headers))
        .omit_empty("models", slice(&key.models, vertex_model))
        .omit_empty("excluded-models", strings(&key.excluded_models))
        .omit_nil("disable-cooling", key.disable_cooling.map(Json::Bool))
        .omit_nil("request-retry", key.request_retry.map(Json::Int))
        .omit_empty("auth-index", string(auth_index))
        .done()
}

/// An `openai-compatibility` entry as the config holds it (upstream's
/// `OpenAICompatibility`).
fn openai_compatibility(entry: &OpenAiCompatibility) -> Json {
    Fields::new()
        .with("name", string(&entry.name))
        .omit_empty("priority", Json::Int(entry.priority))
        .omit_empty("disabled", Json::Bool(entry.disabled))
        .omit_empty("prefix", string(&entry.prefix))
        .with("base-url", string(&entry.base_url))
        .omit_empty(
            "api-key-entries",
            slice(&entry.api_key_entries, |key| {
                openai_compatibility_key(key, "")
            }),
        )
        .with("models", slice(&entry.models, openai_compatibility_model))
        .omit_empty("headers", string_map(&entry.headers))
        .omit_empty(
            "support-prompt-cache-key",
            Json::Bool(entry.support_prompt_cache_key),
        )
        .omit_nil("disable-cooling", entry.disable_cooling.map(Json::Bool))
        .omit_nil("request-retry", entry.request_retry.map(Json::Int))
        .omit_empty(
            "request-scoped-errors",
            slice(&entry.request_scoped_errors, scoped_error),
        )
        .done()
}

/// An `openai-compatibility` entry as the list shows it, with its base URL
/// and API keys trimmed: the credential's `auth-index` on the entry when it
/// has no API keys, else each key's on the key, from `key_indexes` in
/// order (upstream's `openAICompatibilityWithAuthIndex`).
pub(super) fn openai_compatibility_listed(
    entry: &OpenAiCompatibility,
    auth_index: &str,
    key_indexes: &[String],
) -> Json {
    let keys: Vec<Json> = entry
        .api_key_entries
        .iter()
        .enumerate()
        .map(|(i, key)| {
            let trimmed = OpenAiCompatibilityApiKey {
                api_key: key.api_key.trim().to_owned(),
                ..key.clone()
            };
            let index = key_indexes.get(i).map_or("", String::as_str);
            openai_compatibility_key(&trimmed, index)
        })
        .collect();
    Fields::new()
        .with("name", string(&entry.name))
        .omit_empty("priority", Json::Int(entry.priority))
        .with("disabled", Json::Bool(entry.disabled))
        .omit_empty("prefix", string(&entry.prefix))
        .with("base-url", string(entry.base_url.trim()))
        .omit_empty("api-key-entries", Json::Array(keys))
        .omit_empty("models", slice(&entry.models, openai_compatibility_model))
        .omit_empty("headers", string_map(&entry.headers))
        .omit_empty(
            "support-prompt-cache-key",
            Json::Bool(entry.support_prompt_cache_key),
        )
        .omit_nil("disable-cooling", entry.disable_cooling.map(Json::Bool))
        .omit_nil("request-retry", entry.request_retry.map(Json::Int))
        .omit_empty(
            "request-scoped-errors",
            slice(&entry.request_scoped_errors, scoped_error),
        )
        .omit_empty("auth-index", string(auth_index))
        .done()
}

/// One of an `openai-compatibility` entry's API keys (upstream's
/// `OpenAICompatibilityAPIKey` and `openAICompatibilityAPIKeyWithAuthIndex`).
fn openai_compatibility_key(key: &OpenAiCompatibilityApiKey, auth_index: &str) -> Json {
    Fields::new()
        .with("api-key", string(&key.api_key))
        .omit_nil("weight", key.weight.map(Json::Int))
        .omit_empty("proxy-url", string(&key.proxy_url))
        .omit_empty("auth-index", string(auth_index))
        .done()
}

fn gemini_model(model: &GeminiModel) -> Json {
    Fields::new()
        .with("name", string(&model.name))
        .with("alias", string(&model.alias))
        .omit_empty("display-name", string(&model.display_name))
        .omit_empty("max-context-length", Json::Int(model.max_context_length))
        .omit_empty("force-mapping", Json::Bool(model.force_mapping))
        .omit_empty("is-compat", Json::Bool(model.is_compat))
        .omit_nil("thinking", model.thinking.as_ref().map(thinking))
        .done()
}

fn claude_model(model: &ClaudeModel) -> Json {
    Fields::new()
        .with("name", string(&model.name))
        .with("alias", string(&model.alias))
        .omit_empty("display-name", string(&model.display_name))
        .omit_empty("max-context-length", Json::Int(model.max_context_length))
        .omit_empty("force-mapping", Json::Bool(model.force_mapping))
        .omit_empty("is-compat", Json::Bool(model.is_compat))
        .omit_nil("thinking", model.thinking.as_ref().map(thinking))
        .done()
}

fn codex_model(model: &CodexModel) -> Json {
    Fields::new()
        .with("name", string(&model.name))
        .with("alias", string(&model.alias))
        .omit_empty("display-name", string(&model.display_name))
        .omit_empty("max-context-length", Json::Int(model.max_context_length))
        .omit_empty("force-mapping", Json::Bool(model.force_mapping))
        .omit_empty("is-compat", Json::Bool(model.is_compat))
        .omit_empty(
            "support-configuration-update",
            Json::Bool(model.support_configuration_update),
        )
        .omit_nil("thinking", model.thinking.as_ref().map(thinking))
        .done()
}

fn vertex_model(model: &VertexCompatModel) -> Json {
    Fields::new()
        .with("name", string(&model.name))
        .with("alias", string(&model.alias))
        .omit_empty("display-name", string(&model.display_name))
        .omit_empty("force-mapping", Json::Bool(model.force_mapping))
        .omit_nil("thinking", model.thinking.as_ref().map(thinking))
        .done()
}

fn openai_compatibility_model(model: &OpenAiCompatibilityModel) -> Json {
    Fields::new()
        .with("name", string(&model.name))
        .with("alias", string(&model.alias))
        .omit_empty("display-name", string(&model.display_name))
        .omit_empty("max-context-length", Json::Int(model.max_context_length))
        .omit_empty("force-mapping", Json::Bool(model.force_mapping))
        .omit_empty("image", Json::Bool(model.image))
        .omit_empty("input-modalities", strings(&model.input_modalities))
        .omit_empty("output-modalities", strings(&model.output_modalities))
        .omit_empty("is-compat", Json::Bool(model.is_compat))
        .omit_empty(
            "use-max-completion-tokens",
            Json::Bool(model.use_max_completion_tokens),
        )
        .omit_nil("thinking", model.thinking.as_ref().map(thinking))
        .done()
}

/// A model's thinking settings (upstream's `registry.ThinkingSupport`).
fn thinking(thinking: &ThinkingSupport) -> Json {
    thinking_fields(
        thinking.min,
        thinking.max,
        thinking.zero_allowed,
        thinking.dynamic_allowed,
        &thinking.levels,
    )
}

/// The fields of upstream's `registry.ThinkingSupport`, all `omitempty`.
pub(crate) fn thinking_fields(
    min: i64,
    max: i64,
    zero_allowed: bool,
    dynamic_allowed: bool,
    levels: &[String],
) -> Json {
    Fields::new()
        .omit_empty("min", Json::Int(min))
        .omit_empty("max", Json::Int(max))
        .omit_empty("zero_allowed", Json::Bool(zero_allowed))
        .omit_empty("dynamic_allowed", Json::Bool(dynamic_allowed))
        .omit_empty("levels", strings(levels))
        .done()
}

fn scoped_error(rule: &RequestScopedErrorRule) -> Json {
    Fields::new()
        .omit_empty("status", Json::Int(rule.status))
        .omit_empty("match", strings(&rule.matches))
        .omit_empty("match-regexr", strings(&rule.match_regexr))
        .omit_empty("action", string(&rule.action))
        .done()
}

fn model_alias(alias: &OAuthModelAlias) -> Json {
    Fields::new()
        .with("name", string(&alias.name))
        .with("alias", string(&alias.alias))
        .omit_empty("fork", Json::Bool(alias.fork))
        .omit_empty("display-name", string(&alias.display_name))
        .omit_empty("force-mapping", Json::Bool(alias.force_mapping))
        .done()
}

fn model_setting(setting: &OAuthModelSetting) -> Json {
    Fields::new()
        .with("name", string(&setting.name))
        .omit_empty("alias", string(&setting.alias))
        .omit_empty("max-context-length", Json::Int(setting.max_context_length))
        .done()
}
