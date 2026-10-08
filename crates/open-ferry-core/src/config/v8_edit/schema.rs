// Ported from CLIProxyAPI internal/config/config_v8.go (legacyConfig) and
// the structs it holds in internal/config, internal/pluginstore and
// internal/registry (v8.0.20, MIT): their YAML field names, as yaml.v3
// reads them with reflection.
// https://github.com/router-for-me/CLIProxyAPI

//! The field names of upstream's `legacyConfig` and the structs it holds,
//! for [`super::validate`]'s strict decode (`KnownFields`). The tables were
//! generated from v8.0.15's types and checked against v8.0.20's; a field is
//! listed by its YAML key, with the struct it decodes into when that
//! struct's own fields are checked.

/// How a field decodes.
#[derive(Clone, Copy)]
pub(super) enum Field {
    /// A value whose keys aren't checked: a scalar, a list of scalars or a
    /// map.
    Leaf,
    /// A struct (or a pointer to one).
    Struct(&'static Type),
    /// A list of structs.
    List(&'static Type),
    /// A map from names to lists of structs.
    MapList(&'static Type),
}

/// A struct: its Go name, as yaml.v3's errors give it, and its fields.
pub(super) struct Type {
    pub(super) name: &'static str,
    pub(super) fields: &'static [(&'static str, Field)],
}

static CONFIG_ANTIGRAVITY_CONFIG: Type = Type {
    name: "config.AntigravityConfig",
    fields: &[
        (
            "connection-pool",
            Field::Struct(&CONFIG_ANTIGRAVITY_CONNECTION_POOL_CONFIG),
        ),
        ("sensitive-words", Field::Leaf),
    ],
};

static CONFIG_ANTIGRAVITY_CONNECTION_POOL_CONFIG: Type = Type {
    name: "config.AntigravityConnectionPoolConfig",
    fields: &[
        ("enabled", Field::Leaf),
        ("idle-conn-timeout", Field::Leaf),
        ("max-idle-conns-per-host", Field::Leaf),
    ],
};

/// open-ferry's `claude-cli` entries, which upstream doesn't have.
static CONFIG_CLAUDE_CLI: Type = Type {
    name: "config.ClaudeCLI",
    fields: &[
        ("command", Field::Leaf),
        ("config-dir", Field::Leaf),
        ("disabled", Field::Leaf),
        ("excluded-models", Field::Leaf),
        ("max-concurrency", Field::Leaf),
        ("models", Field::List(&CONFIG_CLAUDE_MODEL)),
        ("name", Field::Leaf),
        ("prefix", Field::Leaf),
        ("priority", Field::Leaf),
        ("system-prompt", Field::Leaf),
        ("timeout", Field::Leaf),
        ("weight", Field::Leaf),
    ],
};

static CONFIG_CLAUDE_CODE_CONFIG: Type = Type {
    name: "config.ClaudeCodeConfig",
    fields: &[("disable-cloaking-model-list", Field::Leaf)],
};

static CONFIG_CLAUDE_CONFIG: Type = Type {
    name: "config.ClaudeConfig",
    fields: &[("model-level-cooling", Field::Leaf)],
};

static CONFIG_CLAUDE_HEADER_DEFAULTS: Type = Type {
    name: "config.ClaudeHeaderDefaults",
    fields: &[
        ("arch", Field::Leaf),
        ("os", Field::Leaf),
        ("package-version", Field::Leaf),
        ("runtime-version", Field::Leaf),
        ("stabilize-device-profile", Field::Leaf),
        ("timeout", Field::Leaf),
        ("timezone", Field::Leaf),
        ("user-agent", Field::Leaf),
    ],
};

static CONFIG_CLAUDE_KEY: Type = Type {
    name: "config.ClaudeKey",
    fields: &[
        ("api-key", Field::Leaf),
        ("base-url", Field::Leaf),
        ("cloak", Field::Struct(&CONFIG_CLOAK_CONFIG)),
        ("disable-cooling", Field::Leaf),
        ("excluded-models", Field::Leaf),
        ("experimental-cch-signing", Field::Leaf),
        ("fingerprint-profile", Field::Leaf),
        ("headers", Field::Leaf),
        ("models", Field::List(&CONFIG_CLAUDE_MODEL)),
        ("prefix", Field::Leaf),
        ("priority", Field::Leaf),
        ("proxy-url", Field::Leaf),
        ("rebuild-mid-system-message", Field::Leaf),
        ("request-retry", Field::Leaf),
        (
            "request-scoped-errors",
            Field::List(&CONFIG_REQUEST_SCOPED_ERROR_RULE),
        ),
        ("weight", Field::Leaf),
    ],
};

static CONFIG_CLAUDE_MODEL: Type = Type {
    name: "config.ClaudeModel",
    fields: &[
        ("alias", Field::Leaf),
        ("display-name", Field::Leaf),
        ("force-mapping", Field::Leaf),
        ("is-compat", Field::Leaf),
        ("max-context-length", Field::Leaf),
        ("name", Field::Leaf),
        ("thinking", Field::Struct(&REGISTRY_THINKING_SUPPORT)),
    ],
};

static CONFIG_CLIENT_CONFIG: Type = Type {
    name: "config.ClientConfig",
    fields: &[("codex", Field::Struct(&CONFIG_CODEX_CLIENT_CONFIG))],
};

static CONFIG_CLOAK_CONFIG: Type = Type {
    name: "config.CloakConfig",
    fields: &[
        ("cache-user-id", Field::Leaf),
        ("mode", Field::Leaf),
        ("sensitive-words", Field::Leaf),
        ("strict-mode", Field::Leaf),
    ],
};

static CONFIG_CODEX_CLIENT_CONFIG: Type = Type {
    name: "config.CodexClientConfig",
    fields: &[
        ("enable-apply-patch", Field::Leaf),
        ("optimize-multi-agent-v2", Field::Leaf),
    ],
};

static CONFIG_CODEX_CONFIG: Type = Type {
    name: "config.CodexConfig",
    fields: &[
        ("disable-codex-cloaking", Field::Leaf),
        ("live-media-relay", Field::Leaf),
        ("model-level-cooling", Field::Leaf),
        ("orphan-delegation-compatibility", Field::Leaf),
        ("response-steering", Field::Leaf),
        ("stream-bootstrap-buffering", Field::Leaf),
        ("stream-bootstrap-timeout", Field::Leaf),
    ],
};

static CONFIG_CODEX_HEADER_DEFAULTS: Type = Type {
    name: "config.CodexHeaderDefaults",
    fields: &[("beta-features", Field::Leaf), ("user-agent", Field::Leaf)],
};

static CONFIG_CODEX_KEY: Type = Type {
    name: "config.CodexKey",
    fields: &[
        ("alpha-search", Field::Leaf),
        ("api-key", Field::Leaf),
        ("base-url", Field::Leaf),
        ("disable-codex-cloaking", Field::Leaf),
        ("disable-cooling", Field::Leaf),
        ("excluded-models", Field::Leaf),
        ("headers", Field::Leaf),
        ("models", Field::List(&CONFIG_CODEX_MODEL)),
        ("prefix", Field::Leaf),
        ("priority", Field::Leaf),
        ("proxy-url", Field::Leaf),
        ("request-retry", Field::Leaf),
        (
            "request-scoped-errors",
            Field::List(&CONFIG_REQUEST_SCOPED_ERROR_RULE),
        ),
        ("websockets", Field::Leaf),
        ("weight", Field::Leaf),
    ],
};

static CONFIG_CODEX_MODEL: Type = Type {
    name: "config.CodexModel",
    fields: &[
        ("alias", Field::Leaf),
        ("display-name", Field::Leaf),
        ("force-mapping", Field::Leaf),
        ("is-compat", Field::Leaf),
        ("max-context-length", Field::Leaf),
        ("name", Field::Leaf),
        ("support-configuration-update", Field::Leaf),
        ("thinking", Field::Struct(&REGISTRY_THINKING_SUPPORT)),
    ],
};

static CONFIG_CREDENTIAL_IN_FLIGHT_CONFIG: Type = Type {
    name: "config.CredentialInFlightConfig",
    fields: &[
        ("max-aggregate-groups", Field::Leaf),
        ("max-details", Field::Leaf),
        ("max-part-bytes", Field::Leaf),
        ("max-part-count", Field::Leaf),
        ("max-revision-bytes", Field::Leaf),
        ("max-string-bytes", Field::Leaf),
        ("snapshot-interval", Field::Leaf),
        ("staging-retention", Field::Leaf),
        ("stale-after", Field::Leaf),
    ],
};

static CONFIG_DEVIN_CONFIG: Type = Type {
    name: "config.DevinConfig",
    fields: &[("sensitive-words", Field::Leaf)],
};

static CONFIG_DISCOVERY_CONFIG: Type = Type {
    name: "config.DiscoveryConfig",
    fields: &[
        ("advertise-management", Field::Leaf),
        ("auth-required", Field::Leaf),
        ("enabled", Field::Leaf),
        (
            "interfaces",
            Field::Struct(&CONFIG_DISCOVERY_INTERFACES_CONFIG),
        ),
        ("service-name", Field::Leaf),
        ("service-type", Field::Leaf),
        ("subtypes", Field::Leaf),
    ],
};

static CONFIG_DISCOVERY_INTERFACES_CONFIG: Type = Type {
    name: "config.DiscoveryInterfacesConfig",
    fields: &[("exclude", Field::Leaf), ("include", Field::Leaf)],
};

static CONFIG_GEMINI_KEY: Type = Type {
    name: "config.GeminiKey",
    fields: &[
        ("api-key", Field::Leaf),
        ("base-url", Field::Leaf),
        ("disable-cooling", Field::Leaf),
        ("excluded-models", Field::Leaf),
        ("headers", Field::Leaf),
        ("models", Field::List(&CONFIG_GEMINI_MODEL)),
        ("prefix", Field::Leaf),
        ("priority", Field::Leaf),
        ("proxy-url", Field::Leaf),
        ("request-retry", Field::Leaf),
        (
            "request-scoped-errors",
            Field::List(&CONFIG_REQUEST_SCOPED_ERROR_RULE),
        ),
        ("weight", Field::Leaf),
    ],
};

static CONFIG_GEMINI_MODEL: Type = Type {
    name: "config.GeminiModel",
    fields: &[
        ("alias", Field::Leaf),
        ("display-name", Field::Leaf),
        ("force-mapping", Field::Leaf),
        ("is-compat", Field::Leaf),
        ("max-context-length", Field::Leaf),
        ("name", Field::Leaf),
        ("thinking", Field::Struct(&REGISTRY_THINKING_SUPPORT)),
    ],
};

static CONFIG_OAUTH_MODEL_ALIAS: Type = Type {
    name: "config.OAuthModelAlias",
    fields: &[
        ("alias", Field::Leaf),
        ("display-name", Field::Leaf),
        ("force-mapping", Field::Leaf),
        ("fork", Field::Leaf),
        ("name", Field::Leaf),
    ],
};

static CONFIG_OAUTH_MODEL_SETTING: Type = Type {
    name: "config.OAuthModelSetting",
    fields: &[
        ("alias", Field::Leaf),
        ("max-context-length", Field::Leaf),
        ("name", Field::Leaf),
    ],
};

static CONFIG_OPENAI_COMPATIBILITY: Type = Type {
    name: "config.OpenAICompatibility",
    fields: &[
        (
            "api-key-entries",
            Field::List(&CONFIG_OPENAI_COMPATIBILITY_API_KEY),
        ),
        ("base-url", Field::Leaf),
        ("disable-cooling", Field::Leaf),
        ("disabled", Field::Leaf),
        ("headers", Field::Leaf),
        ("models", Field::List(&CONFIG_OPENAI_COMPATIBILITY_MODEL)),
        ("name", Field::Leaf),
        ("prefix", Field::Leaf),
        ("priority", Field::Leaf),
        ("request-retry", Field::Leaf),
        (
            "request-scoped-errors",
            Field::List(&CONFIG_REQUEST_SCOPED_ERROR_RULE),
        ),
        ("support-prompt-cache-key", Field::Leaf),
    ],
};

static CONFIG_OPENAI_COMPATIBILITY_API_KEY: Type = Type {
    name: "config.OpenAICompatibilityAPIKey",
    fields: &[
        ("api-key", Field::Leaf),
        ("proxy-url", Field::Leaf),
        ("weight", Field::Leaf),
    ],
};

static CONFIG_OPENAI_COMPATIBILITY_MODEL: Type = Type {
    name: "config.OpenAICompatibilityModel",
    fields: &[
        ("alias", Field::Leaf),
        ("display-name", Field::Leaf),
        ("force-mapping", Field::Leaf),
        ("image", Field::Leaf),
        ("input-modalities", Field::Leaf),
        ("is-compat", Field::Leaf),
        ("max-context-length", Field::Leaf),
        ("name", Field::Leaf),
        ("output-modalities", Field::Leaf),
        ("thinking", Field::Struct(&REGISTRY_THINKING_SUPPORT)),
        ("use-max-completion-tokens", Field::Leaf),
    ],
};

static CONFIG_PAYLOAD_CONFIG: Type = Type {
    name: "config.PayloadConfig",
    fields: &[
        ("default", Field::List(&CONFIG_PAYLOAD_RULE)),
        ("default-raw", Field::List(&CONFIG_PAYLOAD_RULE)),
        ("filter", Field::List(&CONFIG_PAYLOAD_FILTER_RULE)),
        ("override", Field::List(&CONFIG_PAYLOAD_RULE)),
        ("override-raw", Field::List(&CONFIG_PAYLOAD_RULE)),
    ],
};

static CONFIG_PAYLOAD_FILTER_RULE: Type = Type {
    name: "config.PayloadFilterRule",
    fields: &[
        ("models", Field::List(&CONFIG_PAYLOAD_MODEL_RULE)),
        ("params", Field::Leaf),
    ],
};

static CONFIG_PAYLOAD_MODEL_RULE: Type = Type {
    name: "config.PayloadModelRule",
    fields: &[
        ("exist", Field::Leaf),
        ("from-protocol", Field::Leaf),
        ("headers", Field::Leaf),
        ("match", Field::Leaf),
        ("name", Field::Leaf),
        ("not-exist", Field::Leaf),
        ("not-match", Field::Leaf),
        ("protocol", Field::Leaf),
    ],
};

static CONFIG_PAYLOAD_RULE: Type = Type {
    name: "config.PayloadRule",
    fields: &[
        ("models", Field::List(&CONFIG_PAYLOAD_MODEL_RULE)),
        ("params", Field::Leaf),
    ],
};

static CONFIG_PLUGINS_CONFIG: Type = Type {
    name: "config.PluginsConfig",
    fields: &[
        ("auth-revision", Field::Leaf),
        ("configs", Field::Leaf),
        ("dir", Field::Leaf),
        ("enabled", Field::Leaf),
        ("store-auth", Field::List(&PLUGINSTORE_AUTH_CONFIG)),
        ("store-sources", Field::Leaf),
    ],
};

static CONFIG_PPROF_CONFIG: Type = Type {
    name: "config.PprofConfig",
    fields: &[("addr", Field::Leaf), ("enable", Field::Leaf)],
};

static CONFIG_QUOTA_EXCEEDED: Type = Type {
    name: "config.QuotaExceeded",
    fields: &[
        ("antigravity-credits", Field::Leaf),
        ("switch-preview-model", Field::Leaf),
        ("switch-project", Field::Leaf),
    ],
};

static CONFIG_REMOTE_MANAGEMENT: Type = Type {
    name: "config.RemoteManagement",
    fields: &[
        ("allow-remote", Field::Leaf),
        ("base-url", Field::Leaf),
        ("disable-auto-update-panel", Field::Leaf),
        ("disable-control-panel", Field::Leaf),
        ("panel-github-repository", Field::Leaf),
        ("secret-key", Field::Leaf),
        // open-ferry's own, which upstream doesn't have.
        ("separate-address", Field::Leaf),
    ],
};

static CONFIG_REQUEST_SCOPED_ERROR_RULE: Type = Type {
    name: "config.RequestScopedErrorRule",
    fields: &[
        ("action", Field::Leaf),
        ("match", Field::Leaf),
        ("match-regexr", Field::Leaf),
        ("status", Field::Leaf),
    ],
};

static CONFIG_ROUTING_CONFIG: Type = Type {
    name: "config.RoutingConfig",
    fields: &[
        ("quota", Field::Struct(&CONFIG_ROUTING_QUOTA)),
        ("session-affinity", Field::Leaf),
        ("session-affinity-subagents", Field::Leaf),
        ("session-affinity-ttl", Field::Leaf),
        ("strategy", Field::Leaf),
    ],
};

/// open-ferry's `routing.quota`, which upstream doesn't have.
static CONFIG_ROUTING_QUOTA: Type = Type {
    name: "config.RoutingQuota",
    fields: &[
        ("check-after", Field::Leaf),
        ("prefer", Field::Leaf),
        ("reserve-percent", Field::Leaf),
    ],
};

/// open-ferry's `self-update`, which upstream doesn't have.
static CONFIG_SELF_UPDATE: Type = Type {
    name: "config.SelfUpdate",
    fields: &[("check-every", Field::Leaf), ("mode", Field::Leaf)],
};

static CONFIG_STREAMING_CONFIG: Type = Type {
    name: "config.StreamingConfig",
    fields: &[
        ("bootstrap-retries", Field::Leaf),
        ("keepalive-seconds", Field::Leaf),
    ],
};

static CONFIG_TLS_CONFIG: Type = Type {
    name: "config.TLSConfig",
    fields: &[
        ("cert", Field::Leaf),
        ("enable", Field::Leaf),
        ("key", Field::Leaf),
    ],
};

static CONFIG_VERTEX_COMPAT_KEY: Type = Type {
    name: "config.VertexCompatKey",
    fields: &[
        ("api-key", Field::Leaf),
        ("base-url", Field::Leaf),
        ("disable-cooling", Field::Leaf),
        ("excluded-models", Field::Leaf),
        ("headers", Field::Leaf),
        ("models", Field::List(&CONFIG_VERTEX_COMPAT_MODEL)),
        ("prefix", Field::Leaf),
        ("priority", Field::Leaf),
        ("proxy-url", Field::Leaf),
        ("request-retry", Field::Leaf),
        ("weight", Field::Leaf),
    ],
};

static CONFIG_VERTEX_COMPAT_MODEL: Type = Type {
    name: "config.VertexCompatModel",
    fields: &[
        ("alias", Field::Leaf),
        ("display-name", Field::Leaf),
        ("force-mapping", Field::Leaf),
        ("name", Field::Leaf),
        ("thinking", Field::Struct(&REGISTRY_THINKING_SUPPORT)),
    ],
};

static CONFIG_XAI_CONFIG: Type = Type {
    name: "config.XAIConfig",
    fields: &[("inject-x-search", Field::Leaf)],
};

pub(super) static CONFIG_LEGACY_CONFIG: Type = Type {
    name: "config.legacyConfig",
    fields: &[
        ("antigravity", Field::Struct(&CONFIG_ANTIGRAVITY_CONFIG)),
        ("antigravity-signature-bypass-strict", Field::Leaf),
        ("antigravity-signature-cache-enabled", Field::Leaf),
        ("api-keys", Field::Leaf),
        ("auth-auto-refresh-workers", Field::Leaf),
        ("auth-dir", Field::Leaf),
        ("claude", Field::Struct(&CONFIG_CLAUDE_CONFIG)),
        ("claude-api-key", Field::List(&CONFIG_CLAUDE_KEY)),
        ("claude-cli", Field::List(&CONFIG_CLAUDE_CLI)),
        ("claude-code", Field::Struct(&CONFIG_CLAUDE_CODE_CONFIG)),
        (
            "claude-header-defaults",
            Field::Struct(&CONFIG_CLAUDE_HEADER_DEFAULTS),
        ),
        ("client", Field::Struct(&CONFIG_CLIENT_CONFIG)),
        ("codex", Field::Struct(&CONFIG_CODEX_CONFIG)),
        ("codex-api-key", Field::List(&CONFIG_CODEX_KEY)),
        (
            "codex-header-defaults",
            Field::Struct(&CONFIG_CODEX_HEADER_DEFAULTS),
        ),
        ("commercial-mode", Field::Leaf),
        ("credential-concurrency", Field::Leaf),
        (
            "credential-in-flight",
            Field::Struct(&CONFIG_CREDENTIAL_IN_FLIGHT_CONFIG),
        ),
        ("debug", Field::Leaf),
        ("devin", Field::Struct(&CONFIG_DEVIN_CONFIG)),
        ("disable-claude-cloak-mode", Field::Leaf),
        ("disable-cooling", Field::Leaf),
        ("disable-image-generation", Field::Leaf),
        ("discovery", Field::Struct(&CONFIG_DISCOVERY_CONFIG)),
        ("error-logs-max-files", Field::Leaf),
        ("force-model-prefix", Field::Leaf),
        ("gemini-api-key", Field::List(&CONFIG_GEMINI_KEY)),
        ("github-token", Field::Leaf),
        ("gpt-image-2-base-model", Field::Leaf),
        ("host", Field::Leaf),
        ("interactions-api-key", Field::List(&CONFIG_GEMINI_KEY)),
        ("logging-to-file", Field::Leaf),
        ("logs-max-total-size-mb", Field::Leaf),
        ("max-retry-credentials", Field::Leaf),
        ("max-retry-interval", Field::Leaf),
        ("meta-api-key", Field::List(&CONFIG_CODEX_KEY)),
        ("models", Field::Struct(&REGISTRY_CATALOG_SOURCES)),
        ("nonstream-keepalive-interval", Field::Leaf),
        ("oauth-excluded-models", Field::Leaf),
        (
            "oauth-model-alias",
            Field::MapList(&CONFIG_OAUTH_MODEL_ALIAS),
        ),
        (
            "oauth-request-scoped-errors",
            Field::MapList(&CONFIG_REQUEST_SCOPED_ERROR_RULE),
        ),
        (
            "oauth-settings",
            Field::MapList(&CONFIG_OAUTH_MODEL_SETTING),
        ),
        (
            "openai-compatibility",
            Field::List(&CONFIG_OPENAI_COMPATIBILITY),
        ),
        ("passthrough-headers", Field::Leaf),
        ("payload", Field::Struct(&CONFIG_PAYLOAD_CONFIG)),
        ("plugins", Field::Struct(&CONFIG_PLUGINS_CONFIG)),
        ("port", Field::Leaf),
        ("pprof", Field::Struct(&CONFIG_PPROF_CONFIG)),
        ("proxy-url", Field::Leaf),
        ("quota-exceeded", Field::Struct(&CONFIG_QUOTA_EXCEEDED)),
        ("redis-usage-queue-retention-seconds", Field::Leaf),
        (
            "remote-management",
            Field::Struct(&CONFIG_REMOTE_MANAGEMENT),
        ),
        ("request-log", Field::Leaf),
        ("request-retry", Field::Leaf),
        ("routing", Field::Struct(&CONFIG_ROUTING_CONFIG)),
        ("save-cooldown-status", Field::Leaf),
        ("self-update", Field::Struct(&CONFIG_SELF_UPDATE)),
        ("streaming", Field::Struct(&CONFIG_STREAMING_CONFIG)),
        ("tls", Field::Struct(&CONFIG_TLS_CONFIG)),
        ("transient-error-cooldown-seconds", Field::Leaf),
        ("trusted-proxies", Field::Leaf),
        ("usage-statistics-enabled", Field::Leaf),
        ("vertex-api-key", Field::List(&CONFIG_VERTEX_COMPAT_KEY)),
        ("video-result-auth-cache-ttl", Field::Leaf),
        ("ws-auth", Field::Leaf),
        ("xai", Field::Struct(&CONFIG_XAI_CONFIG)),
        ("xai-api-key", Field::List(&CONFIG_CODEX_KEY)),
    ],
};

static PLUGINSTORE_AUTH_CONFIG: Type = Type {
    name: "pluginstore.AuthConfig",
    fields: &[
        ("allow-insecure", Field::Leaf),
        ("apply-to", Field::Leaf),
        ("header-name", Field::Leaf),
        ("header-value-env", Field::Leaf),
        ("match", Field::Leaf),
        ("password-env", Field::Leaf),
        ("token-env", Field::Leaf),
        ("type", Field::Leaf),
        ("username-env", Field::Leaf),
    ],
};

static REGISTRY_CATALOG_SOURCES: Type = Type {
    name: "registry.CatalogSources",
    fields: &[
        ("catalog", Field::Leaf),
        ("codex-catalog", Field::Leaf),
        ("devin-catalog", Field::Leaf),
    ],
};

static REGISTRY_THINKING_SUPPORT: Type = Type {
    name: "registry.ThinkingSupport",
    fields: &[
        ("dynamic-allowed", Field::Leaf),
        ("levels", Field::Leaf),
        ("max", Field::Leaf),
        ("min", Field::Leaf),
        ("zero-allowed", Field::Leaf),
    ],
};
