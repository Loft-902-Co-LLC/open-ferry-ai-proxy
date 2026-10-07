// Modelled on the keys of CLIProxyAPI sdk/config's SDKConfig that the HTTP
// handlers read, as internal/api/server_options.go (effectiveSDKConfig) fills
// them (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Settings for the HTTP layer.

use std::time::Duration;

use open_ferry_core::config::{CodexClientConfig, Config, DisableImageGeneration};

/// The default for [`ServerConfig::body_limit`]: 64 MiB.
pub const DEFAULT_BODY_LIMIT: usize = 64 << 20;

/// Settings for the HTTP layer, from the config file.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// The keys clients must present (`api-keys`). Keys are trimmed, and empty
    /// and repeated ones dropped. With none left, every request is let in.
    pub api_keys: Vec<String>,
    /// Whether to pass providers' response headers on to clients
    /// (`passthrough-headers`).
    pub passthrough_headers: bool,
    /// How often to write a newline while a non-streaming call runs
    /// (`nonstream-keepalive-interval`), or `None` for never.
    pub nonstream_keepalive: Option<Duration>,
    /// Stream settings (`streaming`).
    pub streaming: StreamingConfig,
    /// The most bytes a request body may have, before and after decoding.
    /// Upstream has no limit.
    pub body_limit: usize,
    /// Codex client settings (`client.codex`), which shape the model list
    /// Codex clients fetch and whether their collaboration tools are readied
    /// at the Responses boundary.
    pub codex_client: CodexClientConfig,
    /// Whether a Codex sub-agent's orphan delegation outputs become user
    /// messages (`codex.orphan-delegation-compatibility`).
    pub codex_orphan_delegation: bool,
    /// Whether a client's Responses WebSocket keeps one upstream Codex
    /// WebSocket for the whole socket, so that frames sent while a response
    /// runs steer it (`codex.response-steering`, experimental).
    pub codex_response_steering: bool,
    /// The proxies whose forwarded-address headers are believed when a
    /// request's client address is worked out (`trusted-proxies`). They
    /// are read once, when the server starts, as upstream reads them.
    pub trusted_proxies: Vec<String>,
    /// What `disable-image-generation` does; [`DisableImageGeneration::All`]
    /// takes the images endpoints away.
    pub disable_image_generation: DisableImageGeneration,
    /// How long a video's ID stays pinned to the credential that made it
    /// (`video-result-auth-cache-ttl`, 3h by default).
    pub video_auth_ttl: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            api_keys: Vec::new(),
            passthrough_headers: false,
            nonstream_keepalive: None,
            streaming: StreamingConfig::default(),
            body_limit: DEFAULT_BODY_LIMIT,
            codex_client: CodexClientConfig::default(),
            codex_orphan_delegation: false,
            codex_response_steering: false,
            trusted_proxies: Vec::new(),
            disable_image_generation: DisableImageGeneration::Off,
            video_auth_ttl: Config::default().video_result_auth_cache_ttl_duration(),
        }
    }
}

/// Stream settings (`streaming`).
#[derive(Clone, Debug, Default)]
pub struct StreamingConfig {
    /// How often to write an SSE comment once a stream has started
    /// (`keepalive-seconds`), or `None` for never.
    pub keepalive: Option<Duration>,
    /// How many times to restart a stream that failed before its first byte
    /// (`bootstrap-retries`).
    pub bootstrap_retries: usize,
}

impl From<&Config> for ServerConfig {
    /// The settings `config` gives, read as upstream's handlers read them
    /// (`StreamingKeepAliveInterval`, `NonStreamingKeepAliveInterval` and
    /// `StreamingBootstrapRetries`): an interval that isn't positive is
    /// none, and negative retries are 0. The body limit is the default.
    fn from(config: &Config) -> Self {
        let seconds = |seconds: i64| {
            u64::try_from(seconds)
                .ok()
                .filter(|seconds| *seconds > 0)
                .map(Duration::from_secs)
        };
        Self {
            api_keys: config.api_keys.clone(),
            passthrough_headers: config.passthrough_headers,
            nonstream_keepalive: seconds(config.nonstream_keepalive_interval),
            streaming: StreamingConfig {
                keepalive: seconds(config.streaming.keepalive_seconds),
                bootstrap_retries: usize::try_from(config.streaming.bootstrap_retries).unwrap_or(0),
            },
            body_limit: DEFAULT_BODY_LIMIT,
            codex_client: config.client.codex.clone(),
            codex_orphan_delegation: config.codex.orphan_delegation_compatibility,
            codex_response_steering: config.codex.response_steering,
            trusted_proxies: config.trusted_proxies.clone(),
            disable_image_generation: config.disable_image_generation,
            video_auth_ttl: config.video_result_auth_cache_ttl_duration(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_config_as_upstream_does() {
        let config = Config::parse(concat!(
            "api-keys: [k1, k2]\npassthrough-headers: true\n",
            "nonstream-keepalive-interval: 0\n",
            "streaming:\n  keepalive-seconds: 15\n  bootstrap-retries: -1\n",
            "trusted-proxies: [10.0.0.0/8]\n",
        ))
        .unwrap();
        let server = ServerConfig::from(&config);
        assert_eq!(server.api_keys, ["k1", "k2"]);
        assert!(server.passthrough_headers);
        assert_eq!(server.nonstream_keepalive, None);
        assert_eq!(server.streaming.keepalive, Some(Duration::from_secs(15)));
        assert_eq!(server.streaming.bootstrap_retries, 0);
        assert_eq!(server.body_limit, DEFAULT_BODY_LIMIT);
        assert_eq!(server.trusted_proxies, ["10.0.0.0/8"]);
        assert_eq!(server.disable_image_generation, DisableImageGeneration::Off);
        assert_eq!(server.video_auth_ttl, Duration::from_secs(3 * 60 * 60));

        let config =
            Config::parse("nonstream-keepalive-interval: 5\nstreaming:\n  bootstrap-retries: 2\n")
                .unwrap();
        let server = ServerConfig::from(&config);
        assert_eq!(server.nonstream_keepalive, Some(Duration::from_secs(5)));
        assert_eq!(server.streaming.keepalive, None);
        assert_eq!(server.streaming.bootstrap_retries, 2);

        for (text, want) in [
            (
                "disable-image-generation: true",
                DisableImageGeneration::All,
            ),
            (
                "disable-image-generation: chat",
                DisableImageGeneration::Chat,
            ),
        ] {
            let server = ServerConfig::from(&Config::parse(text).unwrap());
            assert_eq!(server.disable_image_generation, want, "{text}");
        }
    }

    // Ported from TestVideoAuthBindingTTLUsesConfig
    // (sdk/api/handlers/openai/openai_videos_handlers_test.go).
    #[test]
    fn reads_the_video_auth_ttl() {
        for (text, want) in [
            ("video-result-auth-cache-ttl: 45m", 45 * 60),
            ("video-result-auth-cache-ttl: invalid", 3 * 60 * 60),
            ("multimedia: {video-result-auth-cache-ttl: 90s}", 90),
        ] {
            let server = ServerConfig::from(&Config::parse(text).unwrap());
            assert_eq!(server.video_auth_ttl, Duration::from_secs(want), "{text}");
        }
        assert_eq!(
            ServerConfig::default().video_auth_ttl,
            Duration::from_secs(3 * 60 * 60)
        );
    }

    // Added: upstream reads the setting as its config loader does, which
    // core's tests cover.
    #[test]
    fn reads_the_orphan_delegation_setting() {
        for (text, want) in [
            ("{}", false),
            ("codex: {orphan-delegation-compatibility: true}", true),
            (
                "oauth: {providers: {codex: {orphan-delegation-compatibility: true}}}",
                true,
            ),
        ] {
            let server = ServerConfig::from(&Config::parse(text).unwrap());
            assert_eq!(server.codex_orphan_delegation, want, "{text}");
        }
    }

    // Added: the handlers read the provider-wide setting, as upstream's
    // effectiveSDKConfig copies it (`CodexResponseSteering`).
    #[test]
    fn reads_the_response_steering_setting() {
        for (text, want) in [
            ("{}", false),
            ("codex: {response-steering: true}", true),
            ("upstream: {codex: {response-steering: true}}", true),
            ("upstream: {codex: {response-steering: false}}", false),
        ] {
            let server = ServerConfig::from(&Config::parse(text).unwrap());
            assert_eq!(server.codex_response_steering, want, "{text}");
        }
    }
}
