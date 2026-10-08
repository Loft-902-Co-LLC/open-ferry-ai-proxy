// Ported from CLIProxyAPI internal/config/config.go, sdk_config.go,
// config_types.go and config_defaults.go, and the strategy names of
// sdk/cliproxy/service_config.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The typed configuration.
//!
//! Field names are upstream's YAML keys in the legacy (pre-v8) layout; a v8
//! document is flattened into that layout before it is decoded. Integers are
//! `i64`, as Go's `int` is on 64-bit targets, and no range is enforced beyond
//! what upstream enforces: a port of 65536 loads.
//!
//! `Debug` output leaves out secrets: API keys, the management key, the
//! GitHub token, header values and proxy URLs, which may carry credentials.
//!
//! Deviations from upstream:
//! - Only the sections listed in the [module docs](super) are typed. The rest
//!   (cloaking, fingerprints, other providers, plugins and so on) are read and
//!   ignored, so values of the wrong type inside them are not errors.
//! - `codex-header-defaults.user-agent` and per-key `disable-codex-cloaking`
//!   are ignored: they only exist to present another client's identity.
//! - Maps are `BTreeMap`s, so they iterate in key order where Go's maps
//!   iterate in random order.
//! - [`ThinkingSupport`] is defined here; upstream's lives in its model
//!   registry.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use open_ferry_translate::go::{equal_fold, to_lower};
use serde::Deserialize;

use super::duration::parse_go_duration;
use super::image_generation::DisableImageGeneration;
use super::model_catalogs::CatalogSources;
use super::payload::PayloadConfig;

/// The auth directory used when `auth-dir` is unset.
pub const DEFAULT_AUTH_DIR: &str = "~/.cli-proxy-api";

/// The repository the management panel is fetched from by default.
pub const DEFAULT_PANEL_GITHUB_REPOSITORY: &str =
    "https://github.com/router-for-me/Cli-Proxy-API-Management-Center";

/// The proxy's configuration, loaded from YAML.
#[derive(Clone, PartialEq, Deserialize)]
#[serde(default, rename = "config.legacyConfig", rename_all = "kebab-case")]
pub struct Config {
    /// Where the model catalogs are read from.
    pub models: CatalogSources,
    /// Client-facing compatibility behavior.
    pub client: ClientConfig,
    /// An optional proxy for outbound requests.
    pub proxy_url: String,
    /// Whether the built-in `image_generation` tool is taken out of the
    /// requests sent upstream, and with `true`, whether the images endpoints
    /// are taken away.
    pub disable_image_generation: DisableImageGeneration,
    /// The main model of a Codex image request that goes through the image
    /// generation tool rather than the Image API. It must start with `gpt-`,
    /// in any case; empty or anything else gives `gpt-5.4-mini`.
    pub gpt_image_2_base_model: String,
    /// Requires explicit model prefixes to reach prefixed credentials.
    pub force_model_prefix: bool,
    /// How long a video's ID stays pinned to the credential that made it,
    /// as a Go duration such as `30m` or `3h`; empty or invalid uses 3h.
    pub video_result_auth_cache_ttl: String,
    /// Enables detailed request logging.
    pub request_log: bool,
    /// Keys clients use to authenticate to this proxy.
    pub api_keys: Vec<String>,
    /// Forwards upstream response headers to clients.
    pub passthrough_headers: bool,
    /// Streaming keep-alives and bootstrap retries.
    pub streaming: StreamingConfig,
    /// Seconds between blank lines sent while a non-streaming response is
    /// pending; `<= 0` disables them.
    pub nonstream_keepalive_interval: i64,
    /// The interface to bind; empty binds all interfaces.
    pub host: String,
    /// The port to listen on.
    pub port: i64,
    /// The token for requests to GitHub's API (upstream's `GitHubToken`),
    /// used over the `GITHUB_TOKEN` environment variable when set. It is
    /// never shown in the management API's config JSON or in logs.
    pub github_token: String,
    /// IPs or CIDRs allowed to set forwarded client IP headers. Loading
    /// checks that each entry parses.
    pub trusted_proxies: Vec<String>,
    /// HTTPS settings.
    pub tls: TlsConfig,
    /// Management API settings.
    pub remote_management: RemoteManagement,
    /// Where credential files are stored, as written; see
    /// [`Config::resolve_auth_dir`].
    pub auth_dir: String,
    /// Enables debug logging.
    pub debug: bool,
    /// Turns off request-log capture and the request-log routes' middleware
    /// for lower memory use (upstream's commercial mode).
    pub commercial_mode: bool,
    /// Writes logs to rotating files instead of stdout.
    pub logging_to_file: bool,
    /// The most megabytes the log directory may hold before the oldest log
    /// files are deleted; 0 disables the limit. Negative values load as 0.
    pub logs_max_total_size_mb: i64,
    /// The most error request log files kept; 0 keeps them all. Defaults to
    /// 10, as do negative values.
    pub error_logs_max_files: i64,
    /// Queues a usage record for each request.
    pub usage_statistics_enabled: bool,
    /// Seconds a queued usage record is kept; loads as 60 when `<= 0` and
    /// at most 3600.
    pub redis_usage_queue_retention_seconds: i64,
    /// Disables credential and model cooldowns unless a credential overrides it.
    pub disable_cooling: bool,
    /// Saves cooldowns next to the credential files, so they survive a
    /// restart.
    pub save_cooldown_status: bool,
    /// Cooldown for transient upstream errors: 0 keeps the default, negative
    /// disables it.
    pub transient_error_cooldown_seconds: i64,
    /// Size of the auth refresh worker pool; `<= 0` uses the default.
    pub auth_auto_refresh_workers: i64,
    /// Additional credential retry rounds after the first.
    pub request_retry: i64,
    /// Most credentials tried per retry round; 0 tries all. Negative values
    /// load as 0.
    pub max_retry_credentials: i64,
    /// Longest cooldown wait, in seconds, before another retry round.
    pub max_retry_interval: i64,
    /// What to do when a quota is exceeded.
    pub quota_exceeded: QuotaExceeded,
    /// Credential selection.
    pub routing: RoutingConfig,
    /// Requires authentication on the WebSocket API. Defaults to true.
    pub ws_auth: bool,
    /// Gemini API keys.
    pub gemini_api_key: Vec<GeminiKey>,
    /// Google Interactions API keys, in the Gemini keys' shape.
    pub interactions_api_key: Vec<GeminiKey>,
    /// Codex API keys.
    pub codex_api_key: Vec<CodexKey>,
    /// xAI API keys, in the Codex keys' shape.
    pub xai_api_key: Vec<CodexKey>,
    /// Meta API keys, in the Codex keys' shape.
    pub meta_api_key: Vec<CodexKey>,
    /// Provider-wide xAI behavior.
    pub xai: XaiConfig,
    /// Provider-wide Codex behavior.
    pub codex: CodexConfig,
    /// Fallback headers for Codex OAuth requests.
    pub codex_header_defaults: CodexHeaderDefaults,
    /// Provider-wide Claude behavior.
    pub claude: ClaudeConfig,
    /// Claude API keys.
    pub claude_api_key: Vec<ClaudeKey>,
    /// Claude Code installations that serve Claude models (`claude-cli`,
    /// not upstream's).
    pub claude_cli: Vec<ClaudeCli>,
    /// OpenAI-compatible upstreams.
    pub openai_compatibility: Vec<OpenAiCompatibility>,
    /// Vertex AI API keys, for Vertex AI's express mode or a service that
    /// takes Vertex AI's paths with an API key.
    pub vertex_api_key: Vec<VertexCompatKey>,
    /// Models excluded per OAuth channel; keys and models are lower case.
    pub oauth_excluded_models: BTreeMap<String, Vec<String>>,
    /// Model aliases per OAuth channel.
    pub oauth_model_alias: BTreeMap<String, Vec<OAuthModelAlias>>,
    /// Request-scoped error rules per OAuth channel.
    pub oauth_request_scoped_errors: BTreeMap<String, Vec<RequestScopedErrorRule>>,
    /// Model settings per OAuth channel.
    pub oauth_settings: BTreeMap<String, Vec<OAuthModelSetting>>,
    /// Rules that edit the payloads sent upstream.
    pub payload: PayloadConfig,
    /// How open-ferry updates itself (`self-update`, not upstream's).
    pub self_update: SelfUpdate,
    /// Legacy names of settings a v8 document placed under
    /// `oauth.providers`, which don't apply to API-key credentials.
    #[serde(skip)]
    pub(crate) oauth_only_fields: BTreeSet<String>,
}

impl Default for Config {
    /// The values upstream sets before decoding.
    fn default() -> Self {
        Self {
            models: CatalogSources::default(),
            client: ClientConfig::default(),
            proxy_url: String::new(),
            disable_image_generation: DisableImageGeneration::Off,
            gpt_image_2_base_model: String::new(),
            force_model_prefix: false,
            video_result_auth_cache_ttl: String::new(),
            request_log: false,
            api_keys: Vec::new(),
            passthrough_headers: false,
            streaming: StreamingConfig::default(),
            nonstream_keepalive_interval: 0,
            host: String::new(),
            port: 0,
            github_token: String::new(),
            trusted_proxies: Vec::new(),
            tls: TlsConfig::default(),
            remote_management: RemoteManagement::default(),
            auth_dir: String::new(),
            debug: false,
            commercial_mode: false,
            logging_to_file: false,
            logs_max_total_size_mb: 0,
            error_logs_max_files: 10,
            usage_statistics_enabled: false,
            redis_usage_queue_retention_seconds: 60,
            disable_cooling: false,
            save_cooldown_status: false,
            transient_error_cooldown_seconds: 0,
            auth_auto_refresh_workers: 0,
            request_retry: 0,
            max_retry_credentials: 0,
            max_retry_interval: 0,
            quota_exceeded: QuotaExceeded::default(),
            routing: RoutingConfig::default(),
            ws_auth: true,
            gemini_api_key: Vec::new(),
            interactions_api_key: Vec::new(),
            codex_api_key: Vec::new(),
            xai_api_key: Vec::new(),
            meta_api_key: Vec::new(),
            xai: XaiConfig::default(),
            codex: CodexConfig::default(),
            codex_header_defaults: CodexHeaderDefaults::default(),
            claude: ClaudeConfig::default(),
            claude_api_key: Vec::new(),
            claude_cli: Vec::new(),
            openai_compatibility: Vec::new(),
            vertex_api_key: Vec::new(),
            oauth_excluded_models: BTreeMap::new(),
            oauth_model_alias: BTreeMap::new(),
            oauth_request_scoped_errors: BTreeMap::new(),
            oauth_settings: BTreeMap::new(),
            payload: PayloadConfig::default(),
            self_update: SelfUpdate::default(),
            oauth_only_fields: BTreeSet::new(),
        }
    }
}

impl Config {
    /// Legacy names of the settings a v8 document set under
    /// `oauth.providers`, such as `ws-auth`.
    pub fn oauth_only_fields(&self) -> &BTreeSet<String> {
        &self.oauth_only_fields
    }

    /// The credential selection strategy.
    pub fn routing_strategy(&self) -> RoutingStrategy {
        match to_lower(self.routing.strategy.trim()).as_str() {
            "weighted-round-robin" | "weightedroundrobin" | "wrr" => {
                RoutingStrategy::WeightedRoundRobin
            }
            "fill-first" | "fillfirst" | "ff" => RoutingStrategy::FillFirst,
            "quota" => RoutingStrategy::Quota,
            _ => RoutingStrategy::RoundRobin,
        }
    }

    /// How long a video's ID stays pinned to the credential that made it:
    /// `video-result-auth-cache-ttl` when it is a positive Go duration,
    /// else three hours (upstream's `videoAuthBindingTTL`).
    pub fn video_result_auth_cache_ttl_duration(&self) -> Duration {
        match parse_go_duration(self.video_result_auth_cache_ttl.trim()) {
            Some(nanos) if nanos > 0 => Duration::from_nanos(nanos.unsigned_abs()),
            _ => Duration::from_secs(3 * 60 * 60),
        }
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("models", &self.models)
            .field("client", &self.client)
            .field("proxy_url", &Redacted(&self.proxy_url))
            .field("disable_image_generation", &self.disable_image_generation)
            .field("gpt_image_2_base_model", &self.gpt_image_2_base_model)
            .field("force_model_prefix", &self.force_model_prefix)
            .field(
                "video_result_auth_cache_ttl",
                &self.video_result_auth_cache_ttl,
            )
            .field("request_log", &self.request_log)
            .field("api_keys", &RedactedList(&self.api_keys))
            .field("passthrough_headers", &self.passthrough_headers)
            .field("streaming", &self.streaming)
            .field(
                "nonstream_keepalive_interval",
                &self.nonstream_keepalive_interval,
            )
            .field("host", &self.host)
            .field("port", &self.port)
            .field("github_token", &Redacted(&self.github_token))
            .field("trusted_proxies", &self.trusted_proxies)
            .field("tls", &self.tls)
            .field("remote_management", &self.remote_management)
            .field("auth_dir", &self.auth_dir)
            .field("debug", &self.debug)
            .field("commercial_mode", &self.commercial_mode)
            .field("logging_to_file", &self.logging_to_file)
            .field("logs_max_total_size_mb", &self.logs_max_total_size_mb)
            .field("error_logs_max_files", &self.error_logs_max_files)
            .field("usage_statistics_enabled", &self.usage_statistics_enabled)
            .field(
                "redis_usage_queue_retention_seconds",
                &self.redis_usage_queue_retention_seconds,
            )
            .field("disable_cooling", &self.disable_cooling)
            .field("save_cooldown_status", &self.save_cooldown_status)
            .field(
                "transient_error_cooldown_seconds",
                &self.transient_error_cooldown_seconds,
            )
            .field("auth_auto_refresh_workers", &self.auth_auto_refresh_workers)
            .field("request_retry", &self.request_retry)
            .field("max_retry_credentials", &self.max_retry_credentials)
            .field("max_retry_interval", &self.max_retry_interval)
            .field("quota_exceeded", &self.quota_exceeded)
            .field("routing", &self.routing)
            .field("ws_auth", &self.ws_auth)
            .field("gemini_api_key", &self.gemini_api_key)
            .field("interactions_api_key", &self.interactions_api_key)
            .field("codex_api_key", &self.codex_api_key)
            .field("xai_api_key", &self.xai_api_key)
            .field("meta_api_key", &self.meta_api_key)
            .field("xai", &self.xai)
            .field("codex", &self.codex)
            .field("codex_header_defaults", &self.codex_header_defaults)
            .field("claude", &self.claude)
            .field("claude_api_key", &self.claude_api_key)
            .field("claude_cli", &self.claude_cli)
            .field("openai_compatibility", &self.openai_compatibility)
            .field("vertex_api_key", &self.vertex_api_key)
            .field("oauth_excluded_models", &self.oauth_excluded_models)
            .field("oauth_model_alias", &self.oauth_model_alias)
            .field(
                "oauth_request_scoped_errors",
                &self.oauth_request_scoped_errors,
            )
            .field("oauth_settings", &self.oauth_settings)
            .field("payload", &self.payload)
            .field("self_update", &self.self_update)
            .field("oauth_only_fields", &self.oauth_only_fields)
            .finish()
    }
}

/// How credentials are picked for a request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RoutingStrategy {
    /// Each credential in turn (`round-robin`, the default).
    #[default]
    RoundRobin,
    /// The first available credential (`fill-first`, `fillfirst`, `ff`).
    FillFirst,
    /// In turn, in proportion to each credential's weight
    /// (`weighted-round-robin`, `weightedroundrobin`, `wrr`).
    WeightedRoundRobin,
    /// By the quota the providers report (`quota`): open-ferry's own, which
    /// CLIProxyAPI runs as round-robin.
    Quota,
}

impl RoutingStrategy {
    /// The canonical name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RoundRobin => "round-robin",
            Self::FillFirst => "fill-first",
            Self::WeightedRoundRobin => "weighted-round-robin",
            Self::Quota => "quota",
        }
    }
}

/// Client-facing compatibility behavior.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "config.ClientConfig", rename_all = "kebab-case")]
pub struct ClientConfig {
    /// Codex client settings.
    pub codex: CodexClientConfig,
}

/// Codex client compatibility.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(
    default,
    rename = "config.CodexClientConfig",
    rename_all = "kebab-case"
)]
pub struct CodexClientConfig {
    /// Optimizes official Codex multi-agent requests across providers.
    pub optimize_multi_agent_v2: bool,
    /// Advertises freeform `apply_patch` for supported models.
    pub enable_apply_patch: bool,
}

/// Streaming behavior.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "config.StreamingConfig", rename_all = "kebab-case")]
pub struct StreamingConfig {
    /// Seconds between SSE heartbeats or WebSocket pings; `<= 0` disables them.
    pub keepalive_seconds: i64,
    /// Retries of a streaming request before any bytes are sent; `<= 0`
    /// disables them.
    pub bootstrap_retries: i64,
}

/// HTTPS server settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "config.TLSConfig", rename_all = "kebab-case")]
pub struct TlsConfig {
    /// Serves HTTPS.
    pub enable: bool,
    /// Certificate file path.
    pub cert: String,
    /// Private key file path.
    pub key: String,
}

/// Management API settings.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "config.RemoteManagement", rename_all = "kebab-case")]
pub struct RemoteManagement {
    /// Allows management access from other hosts.
    pub allow_remote: bool,
    /// The management key as written: plaintext, or a bcrypt hash that
    /// upstream wrote back.
    pub secret_key: String,
    /// Doesn't serve or sync the management panel.
    pub disable_control_panel: bool,
    /// Downloads the panel only when missing, without periodic updates.
    pub disable_auto_update_panel: bool,
    /// Where the panel is fetched from.
    pub panel_github_repository: String,
    /// The management API's base URL, for a remote client.
    pub base_url: String,
    /// open-ferry's own: the `host:port` the management API, the dashboard
    /// and the dashboard API are served on instead of the proxy's address,
    /// or empty to serve them beside the proxy (see
    /// [`RemoteManagement::separate_address`]). Upstream has no such
    /// setting.
    pub separate_address: String,
}

impl Default for RemoteManagement {
    fn default() -> Self {
        Self {
            allow_remote: false,
            secret_key: String::new(),
            disable_control_panel: false,
            disable_auto_update_panel: false,
            panel_github_repository: DEFAULT_PANEL_GITHUB_REPOSITORY.to_owned(),
            base_url: String::new(),
            separate_address: String::new(),
        }
    }
}

impl fmt::Debug for RemoteManagement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteManagement")
            .field("allow_remote", &self.allow_remote)
            .field("secret_key", &Redacted(&self.secret_key))
            .field("disable_control_panel", &self.disable_control_panel)
            .field("disable_auto_update_panel", &self.disable_auto_update_panel)
            .field("panel_github_repository", &self.panel_github_repository)
            .field("base_url", &RedactedUrl(&self.base_url))
            .field("separate_address", &self.separate_address)
            .finish()
    }
}

/// Behavior when a quota is exceeded.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "config.QuotaExceeded", rename_all = "kebab-case")]
pub struct QuotaExceeded {
    /// Switches to another project.
    pub switch_project: bool,
    /// Switches to a preview model.
    pub switch_preview_model: bool,
    /// Falls back to an Antigravity credential with credits.
    pub antigravity_credits: bool,
}

/// Credential selection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "config.RoutingConfig", rename_all = "kebab-case")]
pub struct RoutingConfig {
    /// The strategy as written; see [`Config::routing_strategy`].
    pub strategy: String,
    /// Keeps a session on the credential that served it (upstream's
    /// `SessionAffinity`). A bound credential that becomes unavailable is
    /// always failed over.
    pub session_affinity: bool,
    /// How long a binding lasts, as a Go duration (`30m`, `1h`, `2h30m`);
    /// an hour when empty or not a positive duration.
    pub session_affinity_ttl: String,
    /// Whether a subagent takes its parent session's credential; when
    /// false, subagents spread across the credentials. Unset is true, and
    /// it does nothing without `session_affinity`.
    pub session_affinity_subagents: Option<bool>,
    /// Routing by quota and the cap on long quota rests: open-ferry's own,
    /// which CLIProxyAPI ignores.
    pub quota: RoutingQuota,
}

/// open-ferry's own quota settings (`routing.quota`), which CLIProxyAPI
/// doesn't have.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "config.RoutingQuota", rename_all = "kebab-case")]
pub struct RoutingQuota {
    /// What the `quota` strategy prefers: `soonest-reset` (the default) or
    /// `most-left`.
    pub prefer: String,
    /// The share of each quota window the `quota` strategy keeps back, in
    /// percent: a credential at or past `100 - reserve-percent` in any
    /// window is passed over while another has room. 0 to 100; 0 keeps
    /// none.
    pub reserve_percent: i64,
    /// The longest a quota rest lasts before one request is let through to
    /// check, as a Go duration (`1h`); empty, zero or not a duration rests
    /// until the provider's reset, as upstream does.
    pub check_after: String,
}

/// Provider-wide xAI behavior.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "config.XAIConfig", rename_all = "kebab-case")]
pub struct XaiConfig {
    /// Adds xAI's native `x_search` tool to requests that don't declare it.
    pub inject_x_search: bool,
}

/// Provider-wide Codex behavior.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "config.CodexConfig", rename_all = "kebab-case")]
pub struct CodexConfig {
    /// Holds back stream frames that precede generation so an overloaded
    /// upstream can be retried on another credential.
    pub stream_bootstrap_buffering: bool,
    /// Longest time to hold those frames, as written; see
    /// [`CodexConfig::stream_bootstrap_timeout_duration`].
    pub stream_bootstrap_timeout: String,
    /// Opt-in compatibility for orphan Codex delegation outputs.
    pub orphan_delegation_compatibility: bool,
    /// Scopes quota cooldowns to the requested model.
    pub model_level_cooling: bool,
    /// Experimental full-duplex Codex Responses WebSocket steering: a
    /// Responses WebSocket client's Codex turn on the upstream WebSocket
    /// keeps that connection for the rest of the socket, so the client's
    /// `response.steer` reaches a response while it runs. Keeps one
    /// upstream account and model per socket; accepted input is never
    /// replayed. Off by default.
    pub response_steering: bool,
}

impl CodexConfig {
    /// The bootstrap hold limit; zero means no time limit. Accepts Go
    /// durations (`20s`, `500ms`) and whole seconds (`15`); anything else,
    /// and words such as `none` or `off`, mean zero.
    pub fn stream_bootstrap_timeout_duration(&self) -> Duration {
        const MAX_SECONDS: i64 = i64::MAX / 1_000_000_000;
        let raw = self.stream_bootstrap_timeout.trim();
        let off = ["none", "unlimited", "disabled", "off", "never"];
        if raw.is_empty() || raw == "0" || off.iter().any(|word| equal_fold(raw, word)) {
            return Duration::ZERO;
        }
        if let Some(nanos) = parse_go_duration(raw)
            && let Ok(nanos) = u64::try_from(nanos)
        {
            return Duration::from_nanos(nanos);
        }
        match raw.parse::<i64>() {
            Ok(seconds) if (0..=MAX_SECONDS).contains(&seconds) => {
                Duration::from_secs(seconds.unsigned_abs())
            }
            _ => Duration::ZERO,
        }
    }
}

/// Fallback headers for Codex OAuth requests.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(
    default,
    rename = "config.CodexHeaderDefaults",
    rename_all = "kebab-case"
)]
pub struct CodexHeaderDefaults {
    /// Beta features sent on WebSocket requests when the client sends none.
    pub beta_features: String,
}

/// Provider-wide Claude behavior.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "config.ClaudeConfig", rename_all = "kebab-case")]
pub struct ClaudeConfig {
    /// Scopes quota cooldowns to the requested model.
    pub model_level_cooling: bool,
}

/// A Codex API key and its routing settings.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "config.CodexKey", rename_all = "kebab-case")]
pub struct CodexKey {
    /// The key.
    pub api_key: String,
    /// Selection preference; higher wins.
    pub priority: i64,
    /// Share under weighted round robin: unset means 1, `<= 0` excludes the
    /// key, at most 1,000,000.
    pub weight: Option<i64>,
    /// Namespaces this key's models (`teamA/gpt-5-codex`).
    pub prefix: String,
    /// The endpoint. Keys without one are dropped when loading.
    pub base_url: String,
    /// Uses the Responses WebSocket transport.
    pub websockets: bool,
    /// Lets this key serve the Alpha Search endpoint.
    pub alpha_search: bool,
    /// A proxy for this key, overriding the global one.
    pub proxy_url: String,
    /// Upstream model names and their aliases.
    pub models: Vec<CodexModel>,
    /// Extra headers sent with this key.
    pub headers: BTreeMap<String, String>,
    /// Models this key doesn't serve; lower case.
    pub excluded_models: Vec<String>,
    /// Overrides `disable-cooling` for this key.
    pub disable_cooling: Option<bool>,
    /// Overrides `request-retry`; negative means the global value.
    pub request_retry: Option<i64>,
    /// How upstream errors are classified for this key.
    pub request_scoped_errors: Vec<RequestScopedErrorRule>,
}

impl fmt::Debug for CodexKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodexKey")
            .field("api_key", &Redacted(&self.api_key))
            .field("priority", &self.priority)
            .field("weight", &self.weight)
            .field("prefix", &self.prefix)
            .field("base_url", &RedactedUrl(&self.base_url))
            .field("websockets", &self.websockets)
            .field("alpha_search", &self.alpha_search)
            .field("proxy_url", &Redacted(&self.proxy_url))
            .field("models", &self.models)
            .field("headers", &RedactedMap(&self.headers))
            .field("excluded_models", &self.excluded_models)
            .field("disable_cooling", &self.disable_cooling)
            .field("request_retry", &self.request_retry)
            .field("request_scoped_errors", &self.request_scoped_errors)
            .finish()
    }
}

/// A Codex model served by an API key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "config.CodexModel", rename_all = "kebab-case")]
pub struct CodexModel {
    /// The upstream model name.
    pub name: String,
    /// The name clients use.
    pub alias: String,
    /// A name shown in model lists.
    pub display_name: String,
    /// The context window advertised to Codex clients.
    pub max_context_length: i64,
    /// Rewrites model names in responses back to the alias.
    pub force_mapping: bool,
    /// Converts multi-agent items for Responses-compatible endpoints.
    pub is_compat: bool,
    /// Enables `configuration_update` for this model.
    pub support_configuration_update: bool,
    /// Reasoning support.
    pub thinking: Option<ThinkingSupport>,
}

/// A Claude API key and its routing settings.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "config.ClaudeKey", rename_all = "kebab-case")]
pub struct ClaudeKey {
    /// The key.
    pub api_key: String,
    /// Selection preference; higher wins.
    pub priority: i64,
    /// Share under weighted round robin, as for [`CodexKey::weight`].
    pub weight: Option<i64>,
    /// Namespaces this key's models.
    pub prefix: String,
    /// The endpoint; empty means the default Claude API.
    pub base_url: String,
    /// A proxy for this key, overriding the global one.
    pub proxy_url: String,
    /// Upstream model names and their aliases.
    pub models: Vec<ClaudeModel>,
    /// Extra headers sent with this key.
    pub headers: BTreeMap<String, String>,
    /// Models this key doesn't serve; lower case.
    pub excluded_models: Vec<String>,
    /// Moves mid-conversation system messages into the top-level system field.
    pub rebuild_mid_system_message: bool,
    /// Overrides `disable-cooling` for this key.
    pub disable_cooling: Option<bool>,
    /// Overrides `request-retry`; negative means the global value.
    pub request_retry: Option<i64>,
    /// How upstream errors are classified for this key.
    pub request_scoped_errors: Vec<RequestScopedErrorRule>,
}

impl fmt::Debug for ClaudeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeKey")
            .field("api_key", &Redacted(&self.api_key))
            .field("priority", &self.priority)
            .field("weight", &self.weight)
            .field("prefix", &self.prefix)
            .field("base_url", &RedactedUrl(&self.base_url))
            .field("proxy_url", &Redacted(&self.proxy_url))
            .field("models", &self.models)
            .field("headers", &RedactedMap(&self.headers))
            .field("excluded_models", &self.excluded_models)
            .field(
                "rebuild_mid_system_message",
                &self.rebuild_mid_system_message,
            )
            .field("disable_cooling", &self.disable_cooling)
            .field("request_retry", &self.request_retry)
            .field("request_scoped_errors", &self.request_scoped_errors)
            .finish()
    }
}

/// A Claude model served by an API key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "config.ClaudeModel", rename_all = "kebab-case")]
pub struct ClaudeModel {
    /// The upstream model name.
    pub name: String,
    /// The name clients use.
    pub alias: String,
    /// A name shown in model lists.
    pub display_name: String,
    /// The context window advertised to Codex clients.
    pub max_context_length: i64,
    /// Rewrites model names in responses back to the alias.
    pub force_mapping: bool,
    /// Keeps thinking blocks with empty signatures for compatible upstreams.
    pub is_compat: bool,
    /// Reasoning support.
    pub thinking: Option<ThinkingSupport>,
}

/// A `claude-cli` entry: Claude models served by running the user's own
/// installed Claude Code (`claude -p`) for each request. Not upstream's.
///
/// Anthropic's terms let only Claude Code itself use a Claude subscription
/// sign-in, so open-ferry doesn't sign in or send requests for this
/// provider: the `claude` it runs signs itself in, with the account of its
/// config directory, and makes every request to Anthropic. open-ferry
/// stores no credential for it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "config.ClaudeCLI", rename_all = "kebab-case")]
pub struct ClaudeCli {
    /// The entry's name, required and unique; it seeds the credential's ID
    /// and labels it.
    pub name: String,
    /// The `claude` executable; empty means `claude` on `PATH`.
    pub command: String,
    /// Claude Code's config directory (`CLAUDE_CONFIG_DIR`), which holds
    /// the account it signs in with; empty means Claude Code's default.
    pub config_dir: String,
    /// How the client's system prompt is given to Claude Code: `replace`
    /// (the default) replaces Claude Code's own, `append` adds to it.
    pub system_prompt: String,
    /// How many requests run at once; a request waits for a free slot.
    /// Zero means [`ClaudeCli::DEFAULT_MAX_CONCURRENCY`].
    pub max_concurrency: i64,
    /// How long a request may run, as a Go duration (`10m`); empty means
    /// ten minutes.
    pub timeout: String,
    /// Namespaces this entry's models.
    pub prefix: String,
    /// Model names and their aliases; empty means the built-in Claude
    /// models.
    pub models: Vec<ClaudeModel>,
    /// Models this entry doesn't serve; lower case.
    pub excluded_models: Vec<String>,
    /// Selection preference; higher wins.
    pub priority: i64,
    /// Share under weighted round robin, as for [`CodexKey::weight`].
    pub weight: Option<i64>,
    /// Takes the entry out of routing.
    pub disabled: bool,
}

/// How a `claude-cli` entry gives Claude Code the client's system prompt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClaudeCliSystemPrompt {
    /// `--system-prompt-file`: the client's prompt replaces Claude Code's.
    #[default]
    Replace,
    /// `--append-system-prompt-file`: Claude Code keeps its own prompt and
    /// the client's follows it.
    Append,
}

impl ClaudeCliSystemPrompt {
    /// The mode's config spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Append => "append",
        }
    }

    /// The mode a config spelling names: empty means `replace`; case and
    /// surrounding space are ignored.
    pub fn parse(text: &str) -> Option<Self> {
        match to_lower(text.trim()).as_str() {
            "" | "replace" => Some(Self::Replace),
            "append" => Some(Self::Append),
            _ => None,
        }
    }
}

impl ClaudeCli {
    /// How many requests an entry runs at once when `max-concurrency` is
    /// unset.
    pub const DEFAULT_MAX_CONCURRENCY: usize = 2;

    /// How long a request may run when `timeout` is unset.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

    /// The system prompt mode; an unknown spelling, which loading rejects,
    /// means `replace`.
    pub fn system_prompt_mode(&self) -> ClaudeCliSystemPrompt {
        ClaudeCliSystemPrompt::parse(&self.system_prompt).unwrap_or_default()
    }

    /// How many requests run at once: `max-concurrency` when positive,
    /// else [`ClaudeCli::DEFAULT_MAX_CONCURRENCY`].
    pub fn max_concurrency(&self) -> usize {
        usize::try_from(self.max_concurrency)
            .ok()
            .filter(|limit| *limit > 0)
            .unwrap_or(Self::DEFAULT_MAX_CONCURRENCY)
    }

    /// How long a request may run: `timeout` when it is a positive Go
    /// duration, else [`ClaudeCli::DEFAULT_TIMEOUT`].
    pub fn timeout(&self) -> Duration {
        match parse_go_duration(self.timeout.trim()) {
            Some(nanos) if nanos > 0 => Duration::from_nanos(nanos.unsigned_abs()),
            _ => Self::DEFAULT_TIMEOUT,
        }
    }
}

/// open-ferry's own `self-update` section, which CLIProxyAPI doesn't have:
/// whether and how often open-ferry looks for a newer release of itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "config.SelfUpdate", rename_all = "kebab-case")]
pub struct SelfUpdate {
    /// `auto` (the default) gets a newer release ready, `notify` only says
    /// one is out, and `off` makes no request at all.
    pub mode: String,
    /// How often to look, as a Go duration (`6h`); empty means
    /// [`SelfUpdate::DEFAULT_CHECK_EVERY`].
    pub check_every: String,
}

/// What `self-update.mode` asks for. The order is how much each does, so
/// the lower of two modes is the more cautious.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SelfUpdateMode {
    /// No request of any kind, nothing staged and nothing switched.
    Off,
    /// Looks for a newer release and says when there is one.
    Notify,
    /// Looks for a newer release, and downloads, checks and stages it for
    /// `open-ferry update` to switch to.
    #[default]
    Auto,
}

impl SelfUpdateMode {
    /// The mode's config spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Notify => "notify",
            Self::Auto => "auto",
        }
    }

    /// The mode a spelling names: empty means `auto`; case and surrounding
    /// space are ignored.
    pub fn parse(text: &str) -> Option<Self> {
        match to_lower(text.trim()).as_str() {
            "" | "auto" => Some(Self::Auto),
            "notify" => Some(Self::Notify),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

impl SelfUpdate {
    /// How often to look when `check-every` is empty.
    pub const DEFAULT_CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

    /// The mode; an unknown spelling, which loading rejects, means `off`.
    pub fn mode(&self) -> SelfUpdateMode {
        SelfUpdateMode::parse(&self.mode).unwrap_or(SelfUpdateMode::Off)
    }

    /// How often to look: `check-every` when it is a positive Go duration,
    /// else [`SelfUpdate::DEFAULT_CHECK_EVERY`]. The updater raises a short
    /// one to its minimum.
    pub fn check_every(&self) -> Duration {
        match parse_go_duration(self.check_every.trim()) {
            Some(nanos) if nanos > 0 => Duration::from_nanos(nanos.unsigned_abs()),
            _ => Self::DEFAULT_CHECK_EVERY,
        }
    }
}

/// A Gemini API key and its routing settings.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "config.GeminiKey", rename_all = "kebab-case")]
pub struct GeminiKey {
    /// The key, sent as `x-goog-api-key`.
    pub api_key: String,
    /// Selection preference; higher wins.
    pub priority: i64,
    /// Share under weighted round robin, as for [`CodexKey::weight`].
    pub weight: Option<i64>,
    /// Namespaces this key's models (`teamA/gemini-3-pro-preview`).
    pub prefix: String,
    /// The endpoint; empty means the Gemini API. An entry needs a key or a
    /// base URL, or it is dropped when loading.
    pub base_url: String,
    /// A proxy for this key, overriding the global one.
    pub proxy_url: String,
    /// Upstream model names and their aliases.
    pub models: Vec<GeminiModel>,
    /// Extra headers sent with this key.
    pub headers: BTreeMap<String, String>,
    /// Models this key doesn't serve; lower case.
    pub excluded_models: Vec<String>,
    /// Overrides `disable-cooling` for this key.
    pub disable_cooling: Option<bool>,
    /// Overrides `request-retry`; negative means the global value.
    pub request_retry: Option<i64>,
    /// How upstream errors are classified for this key.
    pub request_scoped_errors: Vec<RequestScopedErrorRule>,
}

impl fmt::Debug for GeminiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeminiKey")
            .field("api_key", &Redacted(&self.api_key))
            .field("priority", &self.priority)
            .field("weight", &self.weight)
            .field("prefix", &self.prefix)
            .field("base_url", &RedactedUrl(&self.base_url))
            .field("proxy_url", &Redacted(&self.proxy_url))
            .field("models", &self.models)
            .field("headers", &RedactedMap(&self.headers))
            .field("excluded_models", &self.excluded_models)
            .field("disable_cooling", &self.disable_cooling)
            .field("request_retry", &self.request_retry)
            .field("request_scoped_errors", &self.request_scoped_errors)
            .finish()
    }
}

/// A Gemini model served by an API key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "config.GeminiModel", rename_all = "kebab-case")]
pub struct GeminiModel {
    /// The upstream model name.
    pub name: String,
    /// The name clients use.
    pub alias: String,
    /// A name shown in model lists.
    pub display_name: String,
    /// The context window advertised to Codex clients.
    pub max_context_length: i64,
    /// Rewrites model names in responses back to the alias.
    pub force_mapping: bool,
    /// Keeps thinking blocks with empty signatures for compatible upstreams.
    pub is_compat: bool,
    /// Reasoning support.
    pub thinking: Option<ThinkingSupport>,
}

/// A Vertex AI API key: for Vertex AI's express mode, or for a service that
/// takes Vertex AI's paths (`/v1/publishers/google/models/...`) with an API
/// key.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "config.VertexCompatKey", rename_all = "kebab-case")]
pub struct VertexCompatKey {
    /// The key, sent as `x-goog-api-key`. Entries without one are dropped
    /// when loading.
    pub api_key: String,
    /// Selection preference; higher wins.
    pub priority: i64,
    /// Share under weighted round robin, as for [`CodexKey::weight`].
    pub weight: Option<i64>,
    /// Namespaces this key's models.
    pub prefix: String,
    /// The endpoint, before `/v1/publishers/...`; empty means Vertex AI.
    pub base_url: String,
    /// A proxy for this key, overriding the global one.
    pub proxy_url: String,
    /// Extra headers sent with this key.
    pub headers: BTreeMap<String, String>,
    /// Upstream model names and their aliases. Only models with both are
    /// kept.
    pub models: Vec<VertexCompatModel>,
    /// Models this key doesn't serve; lower case.
    pub excluded_models: Vec<String>,
    /// Overrides `disable-cooling` for this key.
    pub disable_cooling: Option<bool>,
    /// Overrides `request-retry`; negative means the global value.
    pub request_retry: Option<i64>,
}

impl fmt::Debug for VertexCompatKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VertexCompatKey")
            .field("api_key", &Redacted(&self.api_key))
            .field("priority", &self.priority)
            .field("weight", &self.weight)
            .field("prefix", &self.prefix)
            .field("base_url", &RedactedUrl(&self.base_url))
            .field("proxy_url", &Redacted(&self.proxy_url))
            .field("headers", &RedactedMap(&self.headers))
            .field("models", &self.models)
            .field("excluded_models", &self.excluded_models)
            .field("disable_cooling", &self.disable_cooling)
            .field("request_retry", &self.request_retry)
            .finish()
    }
}

/// A model served by a Vertex AI API key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(
    default,
    rename = "config.VertexCompatModel",
    rename_all = "kebab-case"
)]
pub struct VertexCompatModel {
    /// The upstream model name.
    pub name: String,
    /// The name clients use.
    pub alias: String,
    /// A name shown in model lists.
    pub display_name: String,
    /// Rewrites model names in responses back to the alias.
    pub force_mapping: bool,
    /// Reasoning support.
    pub thinking: Option<ThinkingSupport>,
}

/// An OpenAI-compatible upstream: a Chat Completions endpoint, its keys and
/// the models it serves.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(
    default,
    rename = "config.OpenAICompatibility",
    rename_all = "kebab-case"
)]
pub struct OpenAiCompatibility {
    /// The provider's name, which identifies it in routing and logs.
    pub name: String,
    /// Selection preference; higher wins.
    pub priority: i64,
    /// Takes the provider out of routing.
    pub disabled: bool,
    /// Namespaces this provider's models (`teamA/kimi-k2`).
    pub prefix: String,
    /// The endpoint, up to `/chat/completions`. Providers without one are
    /// dropped when loading.
    pub base_url: String,
    /// The API keys, each with an optional proxy.
    pub api_key_entries: Vec<OpenAiCompatibilityApiKey>,
    /// Upstream model names and their aliases.
    pub models: Vec<OpenAiCompatibilityModel>,
    /// Extra headers sent with every request.
    pub headers: BTreeMap<String, String>,
    /// Passes the client's `prompt_cache_key` on to the provider.
    pub support_prompt_cache_key: bool,
    /// Overrides `disable-cooling` for this provider.
    pub disable_cooling: Option<bool>,
    /// Overrides `request-retry`; negative means the global value.
    pub request_retry: Option<i64>,
    /// How upstream errors are classified for this provider.
    pub request_scoped_errors: Vec<RequestScopedErrorRule>,
}

impl fmt::Debug for OpenAiCompatibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiCompatibility")
            .field("name", &self.name)
            .field("priority", &self.priority)
            .field("disabled", &self.disabled)
            .field("prefix", &self.prefix)
            .field("base_url", &RedactedUrl(&self.base_url))
            .field("api_key_entries", &self.api_key_entries)
            .field("models", &self.models)
            .field("headers", &RedactedMap(&self.headers))
            .field("support_prompt_cache_key", &self.support_prompt_cache_key)
            .field("disable_cooling", &self.disable_cooling)
            .field("request_retry", &self.request_retry)
            .field("request_scoped_errors", &self.request_scoped_errors)
            .finish()
    }
}

/// An API key of an OpenAI-compatible provider.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(
    default,
    rename = "config.OpenAICompatibilityAPIKey",
    rename_all = "kebab-case"
)]
pub struct OpenAiCompatibilityApiKey {
    /// The key.
    pub api_key: String,
    /// Share under weighted round robin, as for [`CodexKey::weight`].
    pub weight: Option<i64>,
    /// A proxy for this key, overriding the global one.
    pub proxy_url: String,
}

impl fmt::Debug for OpenAiCompatibilityApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiCompatibilityApiKey")
            .field("api_key", &Redacted(&self.api_key))
            .field("weight", &self.weight)
            .field("proxy_url", &Redacted(&self.proxy_url))
            .finish()
    }
}

/// A model served by an OpenAI-compatible provider.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(
    default,
    rename = "config.OpenAICompatibilityModel",
    rename_all = "kebab-case"
)]
pub struct OpenAiCompatibilityModel {
    /// The upstream model name.
    pub name: String,
    /// The name clients use.
    pub alias: String,
    /// A name shown in model lists.
    pub display_name: String,
    /// The context window advertised to Codex clients.
    pub max_context_length: i64,
    /// Rewrites model names in responses back to the alias.
    pub force_mapping: bool,
    /// Marks the model as one the `/v1/images/*` endpoints serve; it is
    /// listed with the type `openai-image` and no thinking levels.
    pub image: bool,
    /// What the model takes as chat input, such as `text` and `image`. A
    /// model with `text` and no `image` gets tool results as text.
    pub input_modalities: Vec<String>,
    /// What the model can produce, when known.
    pub output_modalities: Vec<String>,
    /// Keeps thinking blocks with empty signatures for compatible upstreams.
    pub is_compat: bool,
    /// Sends `max_completion_tokens` instead of `max_tokens`.
    pub use_max_completion_tokens: bool,
    /// Reasoning support; unset means the levels `low`, `medium` and `high`.
    pub thinking: Option<ThinkingSupport>,
}

/// A model's reasoning support.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(
    default,
    rename = "registry.ThinkingSupport",
    rename_all = "kebab-case"
)]
pub struct ThinkingSupport {
    /// The smallest thinking budget.
    pub min: i64,
    /// The largest thinking budget.
    pub max: i64,
    /// Whether a budget of 0 turns thinking off.
    pub zero_allowed: bool,
    /// Whether a budget of -1 lets the model decide.
    pub dynamic_allowed: bool,
    /// Named effort levels, when the model uses them.
    pub levels: Vec<String>,
}

/// A rule classifying upstream errors.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(
    default,
    rename = "config.RequestScopedErrorRule",
    rename_all = "kebab-case"
)]
pub struct RequestScopedErrorRule {
    /// The upstream HTTP status.
    pub status: i64,
    /// Substrings of the error body.
    #[serde(rename = "match")]
    pub matches: Vec<String>,
    /// Regular expressions over the error body.
    pub match_regexr: Vec<String>,
    /// `stop`, `stop-and-cooldown`, `continue` or `continue-and-cooldown`;
    /// lower case.
    pub action: String,
}

/// A model alias for an OAuth channel.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "config.OAuthModelAlias", rename_all = "kebab-case")]
pub struct OAuthModelAlias {
    /// The upstream model name.
    pub name: String,
    /// The name clients use.
    pub alias: String,
    /// Lists the alias next to the original model instead of replacing it.
    pub fork: bool,
    /// A name shown in model lists.
    pub display_name: String,
    /// Rewrites model names in responses back to the alias.
    pub force_mapping: bool,
}

/// Model settings for an OAuth channel.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(
    default,
    rename = "config.OAuthModelSetting",
    rename_all = "kebab-case"
)]
pub struct OAuthModelSetting {
    /// The model name.
    pub name: String,
    /// An alias the setting is limited to.
    pub alias: String,
    /// The context window advertised to Codex clients.
    pub max_context_length: i64,
}

impl OAuthModelSetting {
    /// Upstream's `ResolveOAuthModelSetting`: the setting for a model. A
    /// match on the alias beats a match on the name, and later entries beat
    /// earlier ones. Comparisons ignore case and surrounding space.
    pub fn resolve<'a>(
        settings: &'a [OAuthModelSetting],
        model_id: &str,
        metadata_model_id: &str,
        model_name: &str,
    ) -> Option<&'a OAuthModelSetting> {
        let id = to_lower(model_id.trim());
        let meta_id = to_lower(metadata_model_id.trim());
        let name = to_lower(model_name.trim());
        let mut alias_match = None;
        let mut name_match = None;
        for entry in settings {
            let entry_name = to_lower(entry.name.trim());
            if entry_name.is_empty() {
                continue;
            }
            let entry_alias = to_lower(entry.alias.trim());
            if !entry_alias.is_empty() && !id.is_empty() && id == entry_alias {
                alias_match = Some(entry);
            } else if (entry_alias.is_empty() || entry_alias == id)
                && (id == entry_name
                    || (!meta_id.is_empty() && meta_id == entry_name)
                    || (!name.is_empty() && name == entry_name))
            {
                name_match = Some(entry);
            }
        }
        alias_match.or(name_match)
    }
}

/// Shows whether a secret is set without showing it.
pub(crate) struct Redacted<'a>(pub(crate) &'a str);

impl fmt::Debug for Redacted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            f.write_str("\"\"")
        } else {
            f.write_str("<redacted>")
        }
    }
}

/// A URL with its user info, query and fragment hidden, since they may
/// hold secrets.
pub(crate) struct RedactedUrl<'a>(pub(crate) &'a str);

impl fmt::Debug for RedactedUrl<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (url, rest) = match self.0.find(['?', '#']) {
            Some(at) => self.0.split_at(at),
            None => (self.0, ""),
        };
        let authority = url.find("://").map_or(0, |at| at + 3);
        let host_end = url[authority..]
            .find('/')
            .map_or(url.len(), |at| authority + at);
        let mut shown = match url[authority..host_end].rfind('@') {
            Some(at) => format!(
                "{}<redacted>@{}",
                &url[..authority],
                &url[authority + at + 1..]
            ),
            None => url.to_owned(),
        };
        if let Some(delimiter) = rest.chars().next() {
            shown.push(delimiter);
            shown.push_str("<redacted>");
        }
        fmt::Debug::fmt(&shown, f)
    }
}

struct RedactedList<'a>(&'a [String]);

impl fmt::Debug for RedactedList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|item| Redacted(item)))
            .finish()
    }
}

/// Header names with their values hidden.
pub(crate) struct RedactedMap<'a>(pub(crate) &'a BTreeMap<String, String>);

impl fmt::Debug for RedactedMap<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.0.iter().map(|(key, value)| (key, Redacted(value))))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_what_a_base_url_may_hold() {
        let entry = OpenAiCompatibility {
            base_url: "https://review:URL-SECRET@example.invalid/v1?token=QUERY-SECRET#x".into(),
            ..OpenAiCompatibility::default()
        };
        let url = "https://review:URL-SECRET@example.invalid/v1?token=QUERY-SECRET#x";
        let gemini = GeminiKey {
            base_url: url.into(),
            ..GeminiKey::default()
        };
        let vertex = VertexCompatKey {
            base_url: url.into(),
            ..VertexCompatKey::default()
        };
        for shown in [
            format!("{entry:?}"),
            format!("{gemini:?}"),
            format!("{vertex:?}"),
        ] {
            assert!(!shown.contains("SECRET"), "{shown}");
            assert!(
                shown.contains(r#"base_url: "https://<redacted>@example.invalid/v1?<redacted>""#),
                "{shown}"
            );
        }
        for (url, want) in [
            ("", ""),
            ("http://host/v1", "http://host/v1"),
            ("user:pw@host/v1", "<redacted>@host/v1"),
            ("http://host/a@b", "http://host/a@b"),
            ("http://host#k", "http://host#<redacted>"),
        ] {
            assert_eq!(format!("{:?}", RedactedUrl(url)), format!("{want:?}"));
        }
    }

    #[test]
    fn routing_strategy_lowercases_as_go_does() {
        // Go lowers the dotted capital I to a plain i; Rust adds a combining dot.
        let mut config = Config::default();
        config.routing.strategy = "F\u{130}LL-FIRST".to_owned();
        assert_eq!(config.routing_strategy(), RoutingStrategy::FillFirst);
    }

    // Ported from TestVideoAuthBindingTTLUsesConfig
    // (sdk/api/handlers/openai/openai_videos_handlers_test.go), plus the
    // empty, zero and negative cases upstream's code gives 3h.
    #[test]
    fn video_result_auth_cache_ttl_is_a_positive_go_duration() {
        let ttl = |text: &str| {
            Config {
                video_result_auth_cache_ttl: text.to_owned(),
                ..Config::default()
            }
            .video_result_auth_cache_ttl_duration()
        };
        assert_eq!(ttl("45m"), Duration::from_secs(45 * 60));
        assert_eq!(ttl(" 1h30m "), Duration::from_secs(90 * 60));
        for text in ["invalid", "", "0", "0s", "-5m", "30"] {
            assert_eq!(ttl(text), Duration::from_secs(3 * 60 * 60), "{text:?}");
        }
    }

    #[test]
    fn debug_hides_secrets() {
        let mut config = Config {
            api_keys: vec!["sk-client-secret".to_owned()],
            proxy_url: "http://user:pass@proxy".to_owned(),
            ..Config::default()
        };
        config.remote_management.secret_key = "mgmt-secret".to_owned();
        config.codex_api_key.push(CodexKey {
            api_key: "sk-codex-secret".to_owned(),
            headers: BTreeMap::from([("Authorization".to_owned(), "Bearer hdr-secret".to_owned())]),
            ..CodexKey::default()
        });
        config.claude_api_key.push(ClaudeKey {
            api_key: "sk-claude-secret".to_owned(),
            proxy_url: "socks5://u:p@h".to_owned(),
            ..ClaudeKey::default()
        });
        config.gemini_api_key.push(GeminiKey {
            api_key: "gemini-secret".to_owned(),
            proxy_url: "http://gu:gp@proxy".to_owned(),
            headers: BTreeMap::from([("X-Gemini".to_owned(), "gemini-hdr-secret".to_owned())]),
            ..GeminiKey::default()
        });
        config.vertex_api_key.push(VertexCompatKey {
            api_key: "vertex-secret".to_owned(),
            proxy_url: "http://vu:vp@proxy".to_owned(),
            headers: BTreeMap::from([("X-Vertex".to_owned(), "vertex-hdr-secret".to_owned())]),
            ..VertexCompatKey::default()
        });
        config.interactions_api_key.push(GeminiKey {
            api_key: "interactions-secret".to_owned(),
            proxy_url: "http://iu:ip@proxy".to_owned(),
            ..GeminiKey::default()
        });
        config.xai_api_key.push(CodexKey {
            api_key: "xai-secret".to_owned(),
            headers: BTreeMap::from([("X-Xai".to_owned(), "xai-hdr-secret".to_owned())]),
            ..CodexKey::default()
        });
        config.meta_api_key.push(CodexKey {
            api_key: "meta-secret".to_owned(),
            proxy_url: "http://mu:mp@proxy".to_owned(),
            ..CodexKey::default()
        });
        config.openai_compatibility.push(OpenAiCompatibility {
            name: "compat".to_owned(),
            headers: BTreeMap::from([("X-Compat".to_owned(), "compat-hdr-secret".to_owned())]),
            api_key_entries: vec![OpenAiCompatibilityApiKey {
                api_key: "sk-compat-secret".to_owned(),
                proxy_url: "http://cu:cp@proxy".to_owned(),
                weight: Some(2),
            }],
            ..OpenAiCompatibility::default()
        });
        let text = format!("{config:?}");
        for secret in [
            "client-secret",
            "user:pass",
            "mgmt-secret",
            "codex-secret",
            "hdr-secret",
            "claude-secret",
            "u:p@h",
            "compat-secret",
            "compat-hdr-secret",
            "cu:cp",
            "gemini-secret",
            "gu:gp",
            "gemini-hdr-secret",
            "vertex-secret",
            "vu:vp",
            "vertex-hdr-secret",
            "interactions-secret",
            "iu:ip",
            "xai-secret",
            "xai-hdr-secret",
            "meta-secret",
            "mu:mp",
        ] {
            assert!(!text.contains(secret), "{secret} leaked");
        }
        assert!(text.contains("Authorization"));
        assert!(text.contains("X-Compat"));
        assert!(text.contains("X-Gemini"));
        assert!(text.contains("X-Vertex"));
        assert!(text.contains("X-Xai"));
    }

    #[test]
    fn routing_strategy_names() {
        let strategy = |name: &str| {
            let mut config = Config::default();
            config.routing.strategy = name.to_owned();
            config.routing_strategy()
        };
        assert_eq!(strategy(""), RoutingStrategy::RoundRobin);
        assert_eq!(strategy("round-robin"), RoutingStrategy::RoundRobin);
        assert_eq!(strategy(" WRR "), RoutingStrategy::WeightedRoundRobin);
        assert_eq!(
            strategy("weightedroundrobin"),
            RoutingStrategy::WeightedRoundRobin
        );
        assert_eq!(strategy("Fill-First"), RoutingStrategy::FillFirst);
        assert_eq!(strategy("ff"), RoutingStrategy::FillFirst);
        assert_eq!(strategy("random"), RoutingStrategy::RoundRobin);
    }

    #[test]
    fn stream_bootstrap_timeout_parses_like_upstream() {
        let timeout = |raw: &str| {
            CodexConfig {
                stream_bootstrap_timeout: raw.to_owned(),
                ..CodexConfig::default()
            }
            .stream_bootstrap_timeout_duration()
        };
        assert_eq!(timeout(""), Duration::ZERO);
        assert_eq!(timeout("0"), Duration::ZERO);
        assert_eq!(timeout("Never"), Duration::ZERO);
        assert_eq!(timeout("20s"), Duration::from_secs(20));
        assert_eq!(timeout(" 500ms "), Duration::from_millis(500));
        assert_eq!(timeout("1h30m"), Duration::from_secs(5400));
        assert_eq!(timeout("15"), Duration::from_secs(15));
        assert_eq!(timeout("+15"), Duration::from_secs(15));
        assert_eq!(timeout("-5s"), Duration::ZERO);
        assert_eq!(timeout("-5"), Duration::ZERO);
        assert_eq!(timeout("soon"), Duration::ZERO);
        assert_eq!(timeout("9223372037"), Duration::ZERO);
    }

    #[test]
    fn oauth_model_setting_resolution() {
        let setting = |name: &str, alias: &str, length: i64| OAuthModelSetting {
            name: name.to_owned(),
            alias: alias.to_owned(),
            max_context_length: length,
        };
        let settings = vec![
            setting("gpt-5", "", 1),
            setting("gpt-5", "fast", 2),
            setting("GPT-5", "", 3),
            setting(" ", "fast", 4),
        ];
        let length = |id: &str, meta: &str, name: &str| {
            OAuthModelSetting::resolve(&settings, id, meta, name).map(|s| s.max_context_length)
        };
        assert_eq!(length("gpt-5", "", ""), Some(3));
        assert_eq!(length("FAST", "", ""), Some(2));
        assert_eq!(length("other", "gpt-5", ""), Some(3));
        assert_eq!(length("other", "", ""), None);
        assert_eq!(length("", "gpt-5", ""), Some(3));
        assert_eq!(length("", "", "gpt-5"), Some(3));
        assert_eq!(length("nothing", "", ""), None);
        assert_eq!(OAuthModelSetting::resolve(&[], "gpt-5", "", ""), None);
    }

    // oauth_settings_test.go: TestResolveOAuthModelSetting_Priority.
    #[test]
    fn oauth_model_setting_alias_beats_name() {
        let settings = [
            OAuthModelSetting {
                name: "upstream".to_owned(),
                max_context_length: 524_288,
                ..OAuthModelSetting::default()
            },
            OAuthModelSetting {
                name: "upstream".to_owned(),
                alias: "public".to_owned(),
                max_context_length: 1_048_576,
            },
        ];
        let length = |id: &str, meta: &str| {
            OAuthModelSetting::resolve(&settings, id, meta, "").map(|s| s.max_context_length)
        };
        assert_eq!(length("upstream", "upstream"), Some(524_288));
        assert_eq!(length("public", "upstream"), Some(1_048_576));
        assert_eq!(length("unknown", ""), None);
    }
}
