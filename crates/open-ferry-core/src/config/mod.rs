// Ported from CLIProxyAPI internal/config (config.go, sdk_config.go,
// config_types.go, config_load.go, config_v8.go, config_normalization.go,
// parse.go, config_defaults.go and what they call), internal/safemode,
// internal/watcher (watcher.go, config_reload.go, events.go, dispatcher.go)
// and internal/registry/catalog_config.go (CatalogSources) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The proxy's configuration: loading it and watching it for changes.
//!
//! [`Config::load`] reads a YAML file and [`Config::parse`] a payload. Both
//! accept upstream's legacy layout and its v8 layout, or a mix of the two,
//! fill in upstream's defaults and apply its checks and clean-ups. Unknown
//! keys are ignored. Loading never writes the file; [`save`] writes it back,
//! with its comments and the settings this port doesn't type, and
//! [`v8_edit`] edits it in the v8 layout, when a management write asks for
//! it. [`sanitize`] gives the clean-ups loading applies, for the management
//! handlers to apply to what they change.
//!
//! [`ConfigWatcher`] watches the config file and the auth directory and
//! sends a [`WatchEvent`] when the config changes or an auth file is added,
//! changed or removed.
//!
//! [`V8Document`] reads a config file into the v8 layout, as the v8
//! management API shows it.
//!
//! Typed sections: `host`, `port`, `trusted-proxies`, `tls`,
//! `remote-management`, `auth-dir`, `api-keys`, `debug`, `commercial-mode`,
//! `logging-to-file`, `logs-max-total-size-mb`, `error-logs-max-files`,
//! `usage-statistics-enabled`, `redis-usage-queue-retention-seconds`,
//! `request-log`, `proxy-url`, `disable-image-generation`,
//! `passthrough-headers`, `streaming`,
//! `nonstream-keepalive-interval`, `disable-cooling`,
//! `save-cooldown-status`, `transient-error-cooldown-seconds`,
//! `auth-auto-refresh-workers`,
//! `request-retry`, `max-retry-credentials`, `max-retry-interval`,
//! `quota-exceeded`, `routing` (the strategy and session affinity),
//! `ws-auth`, `force-model-prefix`,
//! `video-result-auth-cache-ttl`, `client.codex`, `codex` (minus cloaking
//! and the live media relay), `codex-header-defaults.beta-features`,
//! `claude.model-level-cooling`,
//! `xai`, `gemini-api-key`, `interactions-api-key`, `codex-api-key`,
//! `xai-api-key`, `meta-api-key`, `claude-api-key` (minus `cloak` and
//! `fingerprint-profile`), `openai-compatibility`, `vertex-api-key`,
//! `oauth-excluded-models`, `oauth-model-alias`,
//! `oauth-request-scoped-errors`, `oauth-settings`, `payload` and `models`
//! (the model catalog sources, checked as upstream checks them, and read by
//! [`crate::registry::catalog_sources`]), with
//! their v8 spellings (the key lists as `api-keys.gemini`,
//! `api-keys.vertex` and so on).
//!
//! open-ferry adds `claude-cli` ([`ClaudeCli`]), a top-level list in both
//! layouts that upstream doesn't have.
//!
//! Read and ignored, so they never fail a load except where upstream checks
//! their layout or weights before decoding:
//! - Client impersonation, which this project doesn't do:
//!   `claude-header-defaults`, `codex-header-defaults.user-agent`,
//!   `claude-code`, `disable-claude-cloak-mode`, `codex.disable-codex-cloaking`,
//!   and per-key `cloak`, `fingerprint-profile` and `disable-codex-cloaking`.
//! - Other providers: `antigravity`, `antigravity-signature-*`, `devin`.
//! - Features not ported here: `plugins`, `pprof`, `discovery` and
//!   `codex.live-media-relay`.
//! - Deferred: `credential-concurrency` and `credential-in-flight`.
//!
//! Upstream's `home` section has no YAML form and isn't read.
//!
//! Deviations from upstream:
//! - Loading never writes the file back: upstream replaces a plaintext
//!   management key with its bcrypt hash in the file, and removes legacy
//!   fields that a v8 field overrides. Only [`save`] and [`v8_edit`] write
//!   the file, on a management write.
//! - The ignored sections above aren't typed, so a value of the wrong type
//!   inside them isn't an error.
//! - Each submodule lists its own deviations.

#[cfg(test)]
mod claude_cli_tests;
mod decode;
pub mod diff;
mod duration;
mod image_generation;
mod layout;
mod load;
mod model_catalogs;
mod normalize;
pub(crate) mod paths;
mod payload;
mod safe_mode;
pub mod save;
#[cfg(test)]
mod testing;
mod types;
mod v8;
pub mod v8_edit;
mod watcher;
mod yaml;
pub(crate) mod yaml3;

use std::fmt;

pub(crate) use duration::parse_go_duration;
pub use image_generation::DisableImageGeneration;
pub use layout::{AnyValue, V8Document, YamlTime};
pub use model_catalogs::{CatalogSourceError, CatalogSources, is_url_source};
pub use payload::{PayloadConfig, PayloadFilterRule, PayloadModelRule, PayloadRule};
pub use safe_mode::example_api_key_warning_page;
pub use types::{
    ClaudeCli, ClaudeCliSystemPrompt, ClaudeConfig, ClaudeKey, ClaudeModel, ClientConfig,
    CodexClientConfig, CodexConfig, CodexHeaderDefaults, CodexKey, CodexModel, Config,
    DEFAULT_AUTH_DIR, DEFAULT_PANEL_GITHUB_REPOSITORY, GeminiKey, GeminiModel, OAuthModelAlias,
    OAuthModelSetting, OpenAiCompatibility, OpenAiCompatibilityApiKey, OpenAiCompatibilityModel,
    QuotaExceeded, RemoteManagement, RequestScopedErrorRule, RoutingConfig, RoutingStrategy,
    StreamingConfig, ThinkingSupport, TlsConfig, VertexCompatKey, VertexCompatModel, XaiConfig,
};
pub(crate) use types::{Redacted, RedactedUrl};
pub use watcher::{AuthFile, ConfigWatcher, WatchError, WatchEvent, next_revision};

/// The clean-ups loading applies to a section, as upstream's handlers call
/// them on what a management write changes.
pub mod sanitize {
    pub use super::normalize::{
        META_BASE_URL, normalize_excluded_models, normalize_headers, normalize_model_prefix,
        normalize_oauth_excluded_models, sanitize_claude_cli, sanitize_claude_keys,
        sanitize_codex_keys, sanitize_gemini_keys, sanitize_meta_keys, sanitize_oauth_model_alias,
        sanitize_oauth_request_scoped_errors, sanitize_openai_compatibility, sanitize_vertex_keys,
        sanitize_xai_keys,
    };
}

/// What kind of problem stopped a config from loading.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConfigErrorKind {
    /// The file couldn't be read.
    Read,
    /// The payload was empty.
    Empty,
    /// The text isn't YAML.
    Syntax,
    /// A value has the wrong type, or a key is repeated.
    Decode,
    /// The values don't make a valid config, such as a bad v8 layout, an
    /// out-of-range weight or a trusted proxy that isn't an IP.
    Invalid,
    /// The auth directory starts with `~` and there's no home directory.
    NoHomeDir,
}

/// A config that couldn't be loaded. The message is upstream's wording. It
/// names keys and paths but doesn't quote values, apart from a rejected
/// `trusted-proxies` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    kind: ConfigErrorKind,
    message: String,
}

impl ConfigError {
    pub(crate) fn new(kind: ConfigErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// What kind of problem this is.
    pub fn kind(&self) -> ConfigErrorKind {
        self.kind
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}
