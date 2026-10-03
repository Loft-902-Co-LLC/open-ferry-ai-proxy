// Modelled on the keys of CLIProxyAPI sdk/config's SDKConfig that the HTTP
// handlers read (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Settings for the HTTP layer.

use std::time::Duration;

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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            api_keys: Vec::new(),
            passthrough_headers: false,
            nonstream_keepalive: None,
            streaming: StreamingConfig::default(),
            body_limit: DEFAULT_BODY_LIMIT,
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
