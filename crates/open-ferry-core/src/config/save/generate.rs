// Ported from CLIProxyAPI internal/config/config_types.go, sdk_config.go
// and the config structs they embed (the yaml tags of legacyConfig and its
// fields), disable_image_generation_mode.go (MarshalYAML),
// credential_concurrency.go (WithDefaults), credential_in_flight.go
// (DefaultCredentialInFlightConfig) and config_load.go (the defaults
// LoadConfig sets) (v8.0.20, MIT)
// https://github.com/router-for-me/CLIProxyAPI

//! The settings as upstream writes them: `yaml.Marshal(legacyConfig(cfg))`.
//!
//! [`legacy_config`] lists a [`Config`]'s fields as upstream's
//! `legacyConfig` struct marshals them: in its field order, with
//! `omitempty` applied, pointers written when set, nil slices as `[]` and
//! nil maps as `{}`. The writer marshals that and reads it back into a tree
//! to merge into the file, as upstream does.
//!
//! The sections this port doesn't type (`claude-code`, the
//! credential tuning sections, `plugins`, `pprof`, `discovery`,
//! `antigravity`, `devin`, the Codex live relay, the impersonation header
//! defaults) are written with the values upstream holds when the file
//! doesn't set them. The writer only adds missing keys to those sections
//! (see [`super::merge`]), so what the file has is kept.
//!
//! Deviations from upstream:
//! - The untyped sections above always hold upstream's defaults here,
//!   where upstream's hold what the file set; the merge keeps the file's
//!   values in them instead of writing them back re-rendered.
//! - A payload value that is a mapping with a key that isn't a string, or
//!   a timestamp whose zone is 24 hours or more from UTC, can't be written
//!   (the loader keeps neither); saving fails with an error naming the
//!   rule. Upstream writes them.
//! - open-ferry's `claude-cli` list follows `claude-api-key`. It is always
//!   listed, so emptying it empties the file's list, and the merge doesn't
//!   add an empty one to a file without it.

use std::collections::BTreeMap;

use super::super::image_generation::DisableImageGeneration;
use super::super::layout::AnyValue;
use super::super::payload::{PayloadConfig, PayloadFilterRule, PayloadModelRule, PayloadRule};
use super::super::types::{
    ClaudeCli, ClaudeKey, ClaudeModel, CodexKey, CodexModel, Config, GeminiKey, GeminiModel,
    OAuthModelAlias, OAuthModelSetting, OpenAiCompatibility, OpenAiCompatibilityApiKey,
    OpenAiCompatibilityModel, RequestScopedErrorRule, ThinkingSupport, VertexCompatKey,
    VertexCompatModel,
};
use super::super::yaml3::encode::{Field, Value};
use super::super::yaml3::{Node, TIMESTAMP_TAG};

/// Upstream's `DefaultPprofAddr`.
pub(crate) const DEFAULT_PPROF_ADDR: &str = "127.0.0.1:8316";

/// A value that can't be written, with where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unwritable(pub(crate) String);

/// A struct's fields as they are written.
#[derive(Default)]
struct Fields(Vec<Field>);

impl Fields {
    /// A field written whatever its value.
    fn put(mut self, key: &str, value: Value) -> Self {
        self.0.push(Field::new(key, value));
        self
    }

    /// An `omitempty` field: written unless zero.
    fn omit_empty(self, key: &str, value: Value) -> Self {
        if value.is_zero() {
            self
        } else {
            self.put(key, value)
        }
    }

    /// An `omitempty` pointer field: written when set.
    fn pointer(self, key: &str, value: Option<Value>) -> Self {
        match value {
            Some(value) => self.put(key, value),
            None => self,
        }
    }

    fn done(self) -> Value {
        Value::Struct(self.0)
    }
}

fn s(text: &str) -> Value {
    Value::str(text)
}

fn strings(items: &[String]) -> Value {
    Value::Seq(items.iter().map(|item| s(item)).collect())
}

fn string_map(map: &BTreeMap<String, String>) -> Value {
    Value::Map(map.iter().map(|(k, v)| (s(k), s(v))).collect())
}

fn seq<T>(items: &[T], f: impl Fn(&T) -> Value) -> Value {
    Value::Seq(items.iter().map(f).collect())
}

/// `legacyConfig(cfg)`, as `yaml.Marshal` sees it.
pub(crate) fn legacy_config(cfg: &Config) -> Result<Value, Unwritable> {
    let codex = &cfg.codex;
    let fields = Fields::default()
        // registry.CatalogSources.
        .put(
            "models",
            Fields::default()
                .put("catalog", s(&cfg.models.catalog))
                .put("codex-catalog", s(&cfg.models.codex_catalog))
                .put("devin-catalog", s(&cfg.models.devin_catalog))
                .done(),
        )
        // The inlined SDKConfig.
        .put(
            "client",
            Fields::default()
                .put(
                    "codex",
                    Fields::default()
                        .put(
                            "optimize-multi-agent-v2",
                            Value::Bool(cfg.client.codex.optimize_multi_agent_v2),
                        )
                        .put(
                            "enable-apply-patch",
                            Value::Bool(cfg.client.codex.enable_apply_patch),
                        )
                        .done(),
                )
                .done(),
        )
        .put("proxy-url", s(&cfg.proxy_url))
        .put(
            "disable-image-generation",
            image_generation(cfg.disable_image_generation),
        )
        // gpt-image-2-base-model and video-result-auth-cache-ttl are
        // omitempty and untyped here.
        .put("force-model-prefix", Value::Bool(cfg.force_model_prefix))
        .put("request-log", Value::Bool(cfg.request_log))
        .put(
            "claude-code",
            Fields::default()
                .put("disable-cloaking-model-list", Value::Bool(false))
                .done(),
        )
        .put("api-keys", strings(&cfg.api_keys))
        .put("passthrough-headers", Value::Bool(cfg.passthrough_headers))
        .put(
            "streaming",
            Fields::default()
                .omit_empty(
                    "keepalive-seconds",
                    Value::Int(cfg.streaming.keepalive_seconds),
                )
                .omit_empty(
                    "bootstrap-retries",
                    Value::Int(cfg.streaming.bootstrap_retries),
                )
                .done(),
        )
        .omit_empty(
            "nonstream-keepalive-interval",
            Value::Int(cfg.nonstream_keepalive_interval),
        )
        // Config proper.
        .put("host", s(&cfg.host))
        .put("port", Value::Int(cfg.port))
        .put("github-token", s(&cfg.github_token))
        .put("trusted-proxies", strings(&cfg.trusted_proxies))
        .put(
            "tls",
            Fields::default()
                .put("enable", Value::Bool(cfg.tls.enable))
                .put("cert", s(&cfg.tls.cert))
                .put("key", s(&cfg.tls.key))
                .done(),
        )
        .put("credential-concurrency", credential_concurrency())
        .put("credential-in-flight", credential_in_flight())
        .put(
            "remote-management",
            Fields::default()
                .put(
                    "allow-remote",
                    Value::Bool(cfg.remote_management.allow_remote),
                )
                .put("secret-key", s(&cfg.remote_management.secret_key))
                .put(
                    "disable-control-panel",
                    Value::Bool(cfg.remote_management.disable_control_panel),
                )
                .put(
                    "disable-auto-update-panel",
                    Value::Bool(cfg.remote_management.disable_auto_update_panel),
                )
                .put(
                    "panel-github-repository",
                    s(&cfg.remote_management.panel_github_repository),
                )
                .omit_empty("base-url", s(&cfg.remote_management.base_url))
                .done(),
        )
        .put(
            "plugins",
            Fields::default()
                .put("enabled", Value::Bool(false))
                .put("dir", s("plugins"))
                .put("configs", Value::Map(Vec::new()))
                .done(),
        )
        .put("auth-dir", s(&cfg.auth_dir))
        .put("debug", Value::Bool(cfg.debug))
        .put(
            "pprof",
            Fields::default()
                .put("enable", Value::Bool(false))
                .put("addr", s(DEFAULT_PPROF_ADDR))
                .done(),
        )
        .put("discovery", discovery())
        .put("commercial-mode", Value::Bool(cfg.commercial_mode))
        .put("logging-to-file", Value::Bool(cfg.logging_to_file))
        .put(
            "logs-max-total-size-mb",
            Value::Int(cfg.logs_max_total_size_mb),
        )
        .put("error-logs-max-files", Value::Int(cfg.error_logs_max_files))
        .put(
            "usage-statistics-enabled",
            Value::Bool(cfg.usage_statistics_enabled),
        )
        .put(
            "redis-usage-queue-retention-seconds",
            Value::Int(cfg.redis_usage_queue_retention_seconds),
        )
        .put("disable-cooling", Value::Bool(cfg.disable_cooling))
        .put(
            "save-cooldown-status",
            Value::Bool(cfg.save_cooldown_status),
        )
        .put(
            "transient-error-cooldown-seconds",
            Value::Int(cfg.transient_error_cooldown_seconds),
        )
        .put(
            "auth-auto-refresh-workers",
            Value::Int(cfg.auth_auto_refresh_workers),
        )
        .put("request-retry", Value::Int(cfg.request_retry))
        .put(
            "max-retry-credentials",
            Value::Int(cfg.max_retry_credentials),
        )
        .put("max-retry-interval", Value::Int(cfg.max_retry_interval))
        .put(
            "quota-exceeded",
            Fields::default()
                .put(
                    "switch-project",
                    Value::Bool(cfg.quota_exceeded.switch_project),
                )
                .put(
                    "switch-preview-model",
                    Value::Bool(cfg.quota_exceeded.switch_preview_model),
                )
                .put(
                    "antigravity-credits",
                    Value::Bool(cfg.quota_exceeded.antigravity_credits),
                )
                .done(),
        )
        .put(
            "routing",
            Fields::default()
                .omit_empty("strategy", s(&cfg.routing.strategy))
                .omit_empty(
                    "session-affinity",
                    Value::Bool(cfg.routing.session_affinity),
                )
                .omit_empty("session-affinity-ttl", s(&cfg.routing.session_affinity_ttl))
                .pointer(
                    "session-affinity-subagents",
                    cfg.routing.session_affinity_subagents.map(Value::Bool),
                )
                .done(),
        )
        .put("ws-auth", Value::Bool(cfg.ws_auth))
        // antigravity-signature-cache-enabled and
        // antigravity-signature-bypass-strict are nil pointers here.
        .put("antigravity", Fields::default().done())
        .put("devin", Fields::default().done())
        .put("gemini-api-key", seq(&cfg.gemini_api_key, gemini_key))
        .put(
            "interactions-api-key",
            seq(&cfg.interactions_api_key, gemini_key),
        )
        .put("codex-api-key", seq(&cfg.codex_api_key, codex_key))
        .put("xai-api-key", seq(&cfg.xai_api_key, codex_key))
        .put("meta-api-key", seq(&cfg.meta_api_key, codex_key))
        .put(
            "xai",
            Fields::default()
                .put("inject-x-search", Value::Bool(cfg.xai.inject_x_search))
                .done(),
        )
        .put(
            "codex",
            Fields::default()
                .put("disable-codex-cloaking", Value::Bool(false))
                .put(
                    "stream-bootstrap-buffering",
                    Value::Bool(codex.stream_bootstrap_buffering),
                )
                .omit_empty(
                    "stream-bootstrap-timeout",
                    s(&codex.stream_bootstrap_timeout),
                )
                .put(
                    "orphan-delegation-compatibility",
                    Value::Bool(codex.orphan_delegation_compatibility),
                )
                .put(
                    "model-level-cooling",
                    Value::Bool(codex.model_level_cooling),
                )
                .put("live-media-relay", live_media_relay())
                .put("response-steering", Value::Bool(codex.response_steering))
                .done(),
        )
        .put(
            "codex-header-defaults",
            Fields::default()
                .put("user-agent", s(""))
                .put("beta-features", s(&cfg.codex_header_defaults.beta_features))
                .done(),
        )
        .put(
            "claude",
            Fields::default()
                .put(
                    "model-level-cooling",
                    Value::Bool(cfg.claude.model_level_cooling),
                )
                .done(),
        )
        .put("claude-api-key", seq(&cfg.claude_api_key, claude_key))
        .put("claude-cli", seq(&cfg.claude_cli, claude_cli))
        .put("claude-header-defaults", claude_header_defaults())
        .put("disable-claude-cloak-mode", Value::Bool(false))
        .put(
            "openai-compatibility",
            seq(&cfg.openai_compatibility, openai_compatibility),
        )
        .put("vertex-api-key", seq(&cfg.vertex_api_key, vertex_key))
        .omit_empty(
            "oauth-excluded-models",
            Value::Map(
                cfg.oauth_excluded_models
                    .iter()
                    .map(|(k, v)| (s(k), strings(v)))
                    .collect(),
            ),
        )
        .omit_empty(
            "oauth-model-alias",
            Value::Map(
                cfg.oauth_model_alias
                    .iter()
                    .map(|(k, v)| (s(k), seq(v, oauth_model_alias)))
                    .collect(),
            ),
        )
        .omit_empty(
            "oauth-request-scoped-errors",
            Value::Map(
                cfg.oauth_request_scoped_errors
                    .iter()
                    .map(|(k, v)| (s(k), seq(v, request_scoped_error)))
                    .collect(),
            ),
        )
        .omit_empty(
            "oauth-settings",
            Value::Map(
                cfg.oauth_settings
                    .iter()
                    .map(|(k, v)| (s(k), seq(v, oauth_model_setting)))
                    .collect(),
            ),
        )
        .put("payload", payload(&cfg.payload)?);
    Ok(fields.done())
}

/// The API-key list `legacy_config` writes under the legacy key `old` (one
/// of the v8 key families), or `None` for any other key.
pub(crate) fn family_value(cfg: &Config, old: &str) -> Option<Value> {
    Some(match old {
        "gemini-api-key" => seq(&cfg.gemini_api_key, gemini_key),
        "interactions-api-key" => seq(&cfg.interactions_api_key, gemini_key),
        "vertex-api-key" => seq(&cfg.vertex_api_key, vertex_key),
        "codex-api-key" => seq(&cfg.codex_api_key, codex_key),
        "claude-api-key" => seq(&cfg.claude_api_key, claude_key),
        "xai-api-key" => seq(&cfg.xai_api_key, codex_key),
        "meta-api-key" => seq(&cfg.meta_api_key, codex_key),
        "openai-compatibility" => seq(&cfg.openai_compatibility, openai_compatibility),
        _ => return None,
    })
}

/// `DisableImageGenerationMode.MarshalYAML`.
fn image_generation(mode: DisableImageGeneration) -> Value {
    match mode {
        DisableImageGeneration::Off => Value::Bool(false),
        DisableImageGeneration::All => Value::Bool(true),
        DisableImageGeneration::Chat => s("chat"),
        DisableImageGeneration::Passthrough => s("passthrough"),
    }
}

/// `CredentialConcurrencyConfig.WithDefaults()` of an unset section.
fn credential_concurrency() -> Value {
    const MS: i64 = 1_000_000;
    const S: i64 = 1_000 * MS;
    Fields::default()
        .put("lifecycle-config-revision", Value::Int(0))
        .put("observation-barrier-revision", Value::Int(0))
        .put("cpa-heartbeat-timeout", Value::Duration(3 * S))
        .put("cpa-cancel-bound", Value::Duration(5 * S))
        .put("reclaim-grace", Value::Duration(5 * S))
        .put("cleanup-interval", Value::Duration(5 * S))
        .put("release-flush-interval", Value::Duration(250 * MS))
        .put("release-max-backoff", Value::Duration(2 * S))
        .put("busy-retry-min", Value::Duration(250 * MS))
        .put("busy-retry-max", Value::Duration(S))
        .put("max-limit", Value::Int(1_000_000))
        .done()
}

/// `DefaultCredentialInFlightConfig()`.
fn credential_in_flight() -> Value {
    Fields::default()
        .put("snapshot-interval", s("2s"))
        .put("stale-after", s("10s"))
        .put("max-part-bytes", Value::Int(262_144))
        .put("max-part-count", Value::Int(64))
        .put("max-revision-bytes", Value::Int(16_777_216))
        .put("max-aggregate-groups", Value::Int(100_000))
        .put("max-details", Value::Int(10_000))
        .put("max-string-bytes", Value::Int(256))
        .put("staging-retention", s("1m"))
        .done()
}

/// The discovery section LoadConfig fills in when the file has none.
fn discovery() -> Value {
    let subtypes = [
        "_chat-completions",
        "_responses",
        "_messages",
        "_generate-content",
        "_interactions",
    ];
    Fields::default()
        .put("enabled", Value::Bool(false))
        .put("service-name", s(""))
        .put("service-type", s("_ai-gateway._tcp"))
        .put(
            "subtypes",
            Value::Seq(subtypes.iter().map(|t| s(t)).collect()),
        )
        .put(
            "interfaces",
            Fields::default()
                .put("include", Value::Seq(Vec::new()))
                .put("exclude", Value::Seq(Vec::new()))
                .done(),
        )
        .put("auth-required", Value::Null)
        .put("advertise-management", Value::Bool(false))
        .done()
}

/// An unset `codex.live-media-relay`.
fn live_media_relay() -> Value {
    Fields::default()
        .put("enabled", Value::Bool(false))
        .put("max-sessions", Value::Int(0))
        .put("disable-private-remote-ips", Value::Bool(false))
        .put("public-ip", s(""))
        .put("udp-port-min", Value::Uint(0))
        .put("udp-port-max", Value::Uint(0))
        .put("ice-servers", Value::Seq(Vec::new()))
        .done()
}

/// An unset `claude-header-defaults`.
fn claude_header_defaults() -> Value {
    [
        "user-agent",
        "package-version",
        "runtime-version",
        "os",
        "arch",
        "timeout",
        "timezone",
    ]
    .iter()
    .fold(Fields::default(), |fields, key| fields.put(key, s("")))
    .done()
}

fn thinking(t: &ThinkingSupport) -> Value {
    Fields::default()
        .omit_empty("min", Value::Int(t.min))
        .omit_empty("max", Value::Int(t.max))
        .omit_empty("zero-allowed", Value::Bool(t.zero_allowed))
        .omit_empty("dynamic-allowed", Value::Bool(t.dynamic_allowed))
        .omit_empty("levels", strings(&t.levels))
        .done()
}

fn request_scoped_error(rule: &RequestScopedErrorRule) -> Value {
    Fields::default()
        .omit_empty("status", Value::Int(rule.status))
        .omit_empty("match", strings(&rule.matches))
        .omit_empty("match-regexr", strings(&rule.match_regexr))
        .omit_empty("action", s(&rule.action))
        .done()
}

fn gemini_model(m: &GeminiModel) -> Value {
    Fields::default()
        .put("name", s(&m.name))
        .put("alias", s(&m.alias))
        .omit_empty("display-name", s(&m.display_name))
        .omit_empty("max-context-length", Value::Int(m.max_context_length))
        .omit_empty("force-mapping", Value::Bool(m.force_mapping))
        .omit_empty("is-compat", Value::Bool(m.is_compat))
        .pointer("thinking", m.thinking.as_ref().map(thinking))
        .done()
}

fn gemini_key(k: &GeminiKey) -> Value {
    Fields::default()
        .put("api-key", s(&k.api_key))
        .omit_empty("priority", Value::Int(k.priority))
        .pointer("weight", k.weight.map(Value::Int))
        .omit_empty("prefix", s(&k.prefix))
        .omit_empty("base-url", s(&k.base_url))
        .omit_empty("proxy-url", s(&k.proxy_url))
        .omit_empty("models", seq(&k.models, gemini_model))
        .omit_empty("headers", string_map(&k.headers))
        .omit_empty("excluded-models", strings(&k.excluded_models))
        .pointer("disable-cooling", k.disable_cooling.map(Value::Bool))
        .pointer("request-retry", k.request_retry.map(Value::Int))
        .omit_empty(
            "request-scoped-errors",
            seq(&k.request_scoped_errors, request_scoped_error),
        )
        .done()
}

fn codex_model(m: &CodexModel) -> Value {
    Fields::default()
        .put("name", s(&m.name))
        .put("alias", s(&m.alias))
        .omit_empty("display-name", s(&m.display_name))
        .omit_empty("max-context-length", Value::Int(m.max_context_length))
        .omit_empty("force-mapping", Value::Bool(m.force_mapping))
        .omit_empty("is-compat", Value::Bool(m.is_compat))
        .omit_empty(
            "support-configuration-update",
            Value::Bool(m.support_configuration_update),
        )
        .pointer("thinking", m.thinking.as_ref().map(thinking))
        .done()
}

/// A `CodexKey` (also the xAI and Meta keys). Its `disable-codex-cloaking`
/// pointer is untyped here and left out.
fn codex_key(k: &CodexKey) -> Value {
    Fields::default()
        .put("api-key", s(&k.api_key))
        .omit_empty("priority", Value::Int(k.priority))
        .pointer("weight", k.weight.map(Value::Int))
        .omit_empty("prefix", s(&k.prefix))
        .put("base-url", s(&k.base_url))
        .omit_empty("websockets", Value::Bool(k.websockets))
        .omit_empty("alpha-search", Value::Bool(k.alpha_search))
        .put("proxy-url", s(&k.proxy_url))
        .put("models", seq(&k.models, codex_model))
        .omit_empty("headers", string_map(&k.headers))
        .omit_empty("excluded-models", strings(&k.excluded_models))
        .pointer("disable-cooling", k.disable_cooling.map(Value::Bool))
        .pointer("request-retry", k.request_retry.map(Value::Int))
        .omit_empty(
            "request-scoped-errors",
            seq(&k.request_scoped_errors, request_scoped_error),
        )
        .done()
}

fn claude_model(m: &ClaudeModel) -> Value {
    Fields::default()
        .put("name", s(&m.name))
        .put("alias", s(&m.alias))
        .omit_empty("display-name", s(&m.display_name))
        .omit_empty("max-context-length", Value::Int(m.max_context_length))
        .omit_empty("force-mapping", Value::Bool(m.force_mapping))
        .omit_empty("is-compat", Value::Bool(m.is_compat))
        .pointer("thinking", m.thinking.as_ref().map(thinking))
        .done()
}

/// A `ClaudeKey`. Its `cloak`, `fingerprint-profile` and
/// `experimental-cch-signing` are untyped here and left out.
fn claude_key(k: &ClaudeKey) -> Value {
    Fields::default()
        .put("api-key", s(&k.api_key))
        .omit_empty("priority", Value::Int(k.priority))
        .pointer("weight", k.weight.map(Value::Int))
        .omit_empty("prefix", s(&k.prefix))
        .put("base-url", s(&k.base_url))
        .put("proxy-url", s(&k.proxy_url))
        .put("models", seq(&k.models, claude_model))
        .omit_empty("headers", string_map(&k.headers))
        .omit_empty("excluded-models", strings(&k.excluded_models))
        .omit_empty(
            "rebuild-mid-system-message",
            Value::Bool(k.rebuild_mid_system_message),
        )
        .pointer("disable-cooling", k.disable_cooling.map(Value::Bool))
        .pointer("request-retry", k.request_retry.map(Value::Int))
        .omit_empty(
            "request-scoped-errors",
            seq(&k.request_scoped_errors, request_scoped_error),
        )
        .done()
}

/// A `claude-cli` entry (open-ferry's own).
fn claude_cli(c: &ClaudeCli) -> Value {
    Fields::default()
        .put("name", s(&c.name))
        .omit_empty("command", s(&c.command))
        .omit_empty("config-dir", s(&c.config_dir))
        .omit_empty("system-prompt", s(&c.system_prompt))
        .omit_empty("max-concurrency", Value::Int(c.max_concurrency))
        .omit_empty("timeout", s(&c.timeout))
        .omit_empty("prefix", s(&c.prefix))
        .omit_empty("models", seq(&c.models, claude_model))
        .omit_empty("excluded-models", strings(&c.excluded_models))
        .omit_empty("priority", Value::Int(c.priority))
        .pointer("weight", c.weight.map(Value::Int))
        .omit_empty("disabled", Value::Bool(c.disabled))
        .done()
}

fn openai_model(m: &OpenAiCompatibilityModel) -> Value {
    Fields::default()
        .put("name", s(&m.name))
        .put("alias", s(&m.alias))
        .omit_empty("display-name", s(&m.display_name))
        .omit_empty("max-context-length", Value::Int(m.max_context_length))
        .omit_empty("force-mapping", Value::Bool(m.force_mapping))
        .omit_empty("image", Value::Bool(m.image))
        .omit_empty("input-modalities", strings(&m.input_modalities))
        .omit_empty("output-modalities", strings(&m.output_modalities))
        .omit_empty("is-compat", Value::Bool(m.is_compat))
        .omit_empty(
            "use-max-completion-tokens",
            Value::Bool(m.use_max_completion_tokens),
        )
        .pointer("thinking", m.thinking.as_ref().map(thinking))
        .done()
}

fn openai_api_key(k: &OpenAiCompatibilityApiKey) -> Value {
    Fields::default()
        .put("api-key", s(&k.api_key))
        .pointer("weight", k.weight.map(Value::Int))
        .omit_empty("proxy-url", s(&k.proxy_url))
        .done()
}

fn openai_compatibility(c: &OpenAiCompatibility) -> Value {
    Fields::default()
        .put("name", s(&c.name))
        .omit_empty("priority", Value::Int(c.priority))
        .omit_empty("disabled", Value::Bool(c.disabled))
        .omit_empty("prefix", s(&c.prefix))
        .put("base-url", s(&c.base_url))
        .omit_empty("api-key-entries", seq(&c.api_key_entries, openai_api_key))
        .put("models", seq(&c.models, openai_model))
        .omit_empty("headers", string_map(&c.headers))
        .omit_empty(
            "support-prompt-cache-key",
            Value::Bool(c.support_prompt_cache_key),
        )
        .pointer("disable-cooling", c.disable_cooling.map(Value::Bool))
        .pointer("request-retry", c.request_retry.map(Value::Int))
        .omit_empty(
            "request-scoped-errors",
            seq(&c.request_scoped_errors, request_scoped_error),
        )
        .done()
}

fn vertex_model(m: &VertexCompatModel) -> Value {
    Fields::default()
        .put("name", s(&m.name))
        .put("alias", s(&m.alias))
        .omit_empty("display-name", s(&m.display_name))
        .omit_empty("force-mapping", Value::Bool(m.force_mapping))
        .pointer("thinking", m.thinking.as_ref().map(thinking))
        .done()
}

fn vertex_key(k: &VertexCompatKey) -> Value {
    Fields::default()
        .put("api-key", s(&k.api_key))
        .omit_empty("priority", Value::Int(k.priority))
        .pointer("weight", k.weight.map(Value::Int))
        .omit_empty("prefix", s(&k.prefix))
        .omit_empty("base-url", s(&k.base_url))
        .omit_empty("proxy-url", s(&k.proxy_url))
        .omit_empty("headers", string_map(&k.headers))
        .omit_empty("models", seq(&k.models, vertex_model))
        .omit_empty("excluded-models", strings(&k.excluded_models))
        .pointer("disable-cooling", k.disable_cooling.map(Value::Bool))
        .pointer("request-retry", k.request_retry.map(Value::Int))
        .done()
}

fn oauth_model_alias(a: &OAuthModelAlias) -> Value {
    Fields::default()
        .put("name", s(&a.name))
        .put("alias", s(&a.alias))
        .omit_empty("fork", Value::Bool(a.fork))
        .omit_empty("display-name", s(&a.display_name))
        .omit_empty("force-mapping", Value::Bool(a.force_mapping))
        .done()
}

fn oauth_model_setting(m: &OAuthModelSetting) -> Value {
    Fields::default()
        .put("name", s(&m.name))
        .omit_empty("alias", s(&m.alias))
        .omit_empty("max-context-length", Value::Int(m.max_context_length))
        .done()
}

fn payload(p: &PayloadConfig) -> Result<Value, Unwritable> {
    let rules = |rules: &[PayloadRule], name: &str| -> Result<Value, Unwritable> {
        rules
            .iter()
            .enumerate()
            .map(|(i, rule)| payload_rule(rule, &format!("payload.{name}[{i}]")))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Seq)
    };
    Ok(Fields::default()
        .put("default", rules(&p.default, "default")?)
        .put("default-raw", rules(&p.default_raw, "default-raw")?)
        .put("override", rules(&p.r#override, "override")?)
        .put("override-raw", rules(&p.override_raw, "override-raw")?)
        .put(
            "filter",
            p.filter
                .iter()
                .enumerate()
                .map(|(i, rule)| payload_filter(rule, &format!("payload.filter[{i}]")))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Seq)?,
        )
        .done())
}

fn payload_rule(rule: &PayloadRule, at: &str) -> Result<Value, Unwritable> {
    let params = rule
        .params
        .iter()
        .map(|(k, v)| Ok((s(k), any_value(v, at)?)))
        .collect::<Result<Vec<_>, Unwritable>>()?;
    Ok(Fields::default()
        .put("models", payload_models(&rule.models, at)?)
        .put("params", Value::Map(params))
        .done())
}

fn payload_filter(rule: &PayloadFilterRule, at: &str) -> Result<Value, Unwritable> {
    Ok(Fields::default()
        .put("models", payload_models(&rule.models, at)?)
        .put("params", strings(&rule.params))
        .done())
}

fn payload_models(models: &[PayloadModelRule], at: &str) -> Result<Value, Unwritable> {
    models
        .iter()
        .map(|m| payload_model(m, at))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Seq)
}

fn payload_model(m: &PayloadModelRule, at: &str) -> Result<Value, Unwritable> {
    let conditions = |items: &[BTreeMap<String, AnyValue>]| -> Result<Value, Unwritable> {
        items
            .iter()
            .map(|map| {
                map.iter()
                    .map(|(k, v)| Ok((s(k), any_value(v, at)?)))
                    .collect::<Result<Vec<_>, Unwritable>>()
                    .map(Value::Map)
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Seq)
    };
    Ok(Fields::default()
        .put("name", s(&m.name))
        .put("protocol", s(&m.protocol))
        .put("headers", string_map(&m.headers))
        .put("from-protocol", s(&m.from_protocol))
        .put("match", conditions(&m.r#match)?)
        .put("not-match", conditions(&m.not_match)?)
        .put("exist", strings(&m.exist))
        .put("not-exist", strings(&m.not_exist))
        .done())
}

/// A value decoded into Go's `any`, as yaml.v3 marshals it back.
fn any_value(value: &AnyValue, at: &str) -> Result<Value, Unwritable> {
    Ok(match value {
        AnyValue::Null => Value::Null,
        AnyValue::Bool(v) => Value::Bool(*v),
        AnyValue::Int(v) => Value::Int(*v),
        AnyValue::Uint(v) => Value::Uint(*v),
        AnyValue::Float(v) => Value::Float(*v),
        AnyValue::Str(v) => s(v),
        // encoder.timev: the RFC 3339 text, plain.
        AnyValue::Time(Some(text), _) => Value::Node(Node::scalar(TIMESTAMP_TAG, text)),
        AnyValue::Time(None, _) => {
            return Err(Unwritable(format!(
                "{at}: a timestamp with a zone 24 hours or more from UTC can't be written"
            )));
        }
        AnyValue::Seq(items) => Value::Seq(
            items
                .iter()
                .map(|item| any_value(item, at))
                .collect::<Result<_, _>>()?,
        ),
        AnyValue::Map(map) => Value::Map(
            map.iter()
                .map(|(k, v)| Ok((s(k), any_value(v, at)?)))
                .collect::<Result<_, Unwritable>>()?,
        ),
        AnyValue::AnyMap => {
            return Err(Unwritable(format!(
                "{at}: a mapping with a key that isn't a string can't be written"
            )));
        }
    })
}
