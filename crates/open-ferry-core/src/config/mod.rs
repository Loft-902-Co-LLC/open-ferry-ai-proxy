// Ported from CLIProxyAPI internal/config (config.go, sdk_config.go,
// config_types.go, config_load.go, config_v8.go, config_normalization.go,
// parse.go, config_defaults.go and what they call), internal/safemode and
// internal/watcher (watcher.go, config_reload.go, events.go, dispatcher.go)
// (v8.0.10, MIT; config_v8.go and oauth_scope.go as of v8.0.11).
// https://github.com/router-for-me/CLIProxyAPI

//! The proxy's configuration: loading it and watching it for changes.
//!
//! [`Config::load`] reads a YAML file and [`Config::parse`] a payload. Both
//! accept upstream's legacy layout and its v8 layout, or a mix of the two,
//! fill in upstream's defaults and apply its checks and clean-ups. Unknown
//! keys are ignored. The file is never written.
//!
//! [`ConfigWatcher`] watches the config file and the auth directory and
//! sends a [`WatchEvent`] when the config changes or an auth file is added,
//! changed or removed.
//!
//! Typed sections: `host`, `port`, `trusted-proxies`, `tls`,
//! `remote-management`, `auth-dir`, `api-keys`, `debug`, `logging-to-file`,
//! `request-log`, `proxy-url`, `passthrough-headers`, `streaming`,
//! `nonstream-keepalive-interval`, `disable-cooling`,
//! `transient-error-cooldown-seconds`, `auth-auto-refresh-workers`,
//! `request-retry`, `max-retry-credentials`, `max-retry-interval`,
//! `quota-exceeded`, `routing.strategy`, `ws-auth`, `force-model-prefix`,
//! `client.codex`, `codex` (minus cloaking and the live media relay),
//! `codex-header-defaults.beta-features`, `claude.model-level-cooling`,
//! `codex-api-key`, `claude-api-key` (minus `cloak` and
//! `fingerprint-profile`), `oauth-excluded-models`, `oauth-model-alias`,
//! `oauth-request-scoped-errors` and `oauth-settings`, with their v8
//! spellings.
//!
//! Read and ignored, so they never fail a load except where upstream checks
//! their layout or weights before decoding:
//! - Client impersonation, which this project doesn't do:
//!   `claude-header-defaults`, `codex-header-defaults.user-agent`,
//!   `claude-code`, `disable-claude-cloak-mode`, `codex.disable-codex-cloaking`,
//!   and per-key `cloak`, `fingerprint-profile` and `disable-codex-cloaking`.
//! - Session affinity: `routing.session-affinity`,
//!   `routing.session-affinity-ttl` and `routing.session-affinity-subagents`.
//! - Other providers: `gemini-api-key`, `interactions-api-key`,
//!   `vertex-api-key`, `xai-api-key`, `meta-api-key`, `openai-compatibility`,
//!   `xai`, `antigravity`, `antigravity-signature-*`, `devin`.
//! - Features not ported here: `plugins`, `pprof`, `discovery`,
//!   `commercial-mode`, `payload`, `disable-image-generation`,
//!   `gpt-image-2-base-model`, `video-result-auth-cache-ttl`,
//!   `codex.live-media-relay`, `save-cooldown-status`,
//!   `usage-statistics-enabled`, `redis-usage-queue-retention-seconds`,
//!   `logs-max-total-size-mb`, `error-logs-max-files`.
//! - Deferred: `credential-concurrency` and `credential-in-flight`.
//!
//! Upstream's `home` section has no YAML form and isn't read.
//!
//! Deviations from upstream:
//! - Nothing is written back to the file (no bcrypt hashing of the
//!   management key, no removal of overridden legacy fields).
//! - The ignored sections above aren't typed, so a value of the wrong type
//!   inside them isn't an error.
//! - Each submodule lists its own deviations.

mod decode;
mod duration;
mod load;
mod normalize;
mod paths;
mod safe_mode;
#[cfg(test)]
mod testing;
mod types;
mod v8;
mod watcher;
mod yaml;

use std::fmt;

pub(crate) use duration::parse_go_duration;
pub use safe_mode::example_api_key_warning_page;
pub use types::{
    ClaudeConfig, ClaudeKey, ClaudeModel, ClientConfig, CodexClientConfig, CodexConfig,
    CodexHeaderDefaults, CodexKey, CodexModel, Config, DEFAULT_AUTH_DIR,
    DEFAULT_PANEL_GITHUB_REPOSITORY, OAuthModelAlias, OAuthModelSetting, QuotaExceeded,
    RemoteManagement, RequestScopedErrorRule, RoutingConfig, RoutingStrategy, StreamingConfig,
    ThinkingSupport, TlsConfig,
};
pub use watcher::{ConfigWatcher, WatchError, WatchEvent};

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
