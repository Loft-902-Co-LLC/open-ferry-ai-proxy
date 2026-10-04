// Ported from CLIProxyAPI sdk/proxyutil/proxy.go (Parse, BuildHTTPTransport,
// Redact) and internal/runtime/executor/helps/proxy_helpers.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! HTTP clients for Codex calls, one per proxy, and bounded body reads. The
//! OpenAI-compatible executor uses them too.
//!
//! A credential's `proxy_url` wins over the global one. A proxy setting is
//! empty (use the environment's proxy, as Go's default transport does),
//! `direct` or `none` (no proxy), or an `http` or `https` proxy URL.
//!
//! Deviations from upstream:
//! - Redirects are followed only within the first request's origin; see
//!   [`crate::redirect`].
//! - Clients are kept and shared per proxy URL; upstream builds a client per
//!   request.
//! - `socks5` and `socks5h` proxies aren't supported yet: the client is
//!   built without reqwest's `socks` feature. Such a setting is reported and
//!   treated like an invalid one.
//! - An invalid proxy falls back to the global proxy's client, then to the
//!   environment's proxy; upstream falls back to a transport the caller may
//!   put in the request context, then to the environment's proxy.
//! - The per-request proxy override (`RequestProxyURL`) isn't ported.
//! - Response bodies are read up to a limit.

use std::collections::HashMap;
use std::error::Error as StdError;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// What our requests call themselves, unless the client sent its own.
pub const USER_AGENT: &str = concat!("open-ferry/", env!("CARGO_PKG_VERSION"));

/// How long to wait for a connection, as Go's default dialer does.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an idle connection is kept, as in Go's default transport.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// TCP keep-alive, as Go's default dialer sets it.
const TCP_KEEPALIVE: Duration = Duration::from_secs(30);

/// A parsed proxy setting (upstream's `proxyutil.Setting`).
#[derive(Clone, Debug, PartialEq, Eq)]
enum ProxySetting {
    /// Empty: use the environment's proxy.
    Inherit,
    /// `direct` or `none`: no proxy at all.
    Direct,
    /// Through this proxy.
    Proxy(String),
}

/// Parses a proxy setting (upstream's `proxyutil.Parse`).
fn parse_proxy(raw: &str) -> Result<ProxySetting, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(ProxySetting::Inherit);
    }
    if trimmed.eq_ignore_ascii_case("direct") || trimmed.eq_ignore_ascii_case("none") {
        return Ok(ProxySetting::Direct);
    }
    let Ok(parsed) = url::Url::parse(trimmed) else {
        return Err("parse proxy URL failed".to_owned());
    };
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err("proxy URL missing scheme/host".to_owned());
    }
    match parsed.scheme() {
        "http" | "https" => Ok(ProxySetting::Proxy(trimmed.to_owned())),
        "socks5" | "socks5h" => Err(format!(
            "unsupported proxy scheme: {} (not supported yet)",
            parsed.scheme()
        )),
        scheme => Err(format!("unsupported proxy scheme: {scheme}")),
    }
}

/// A proxy URL with its credentials hidden, for logs (`proxyutil.Redact`).
pub(crate) fn redact_proxy_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let Ok(parsed) = url::Url::parse(trimmed) else {
        return "<invalid proxy URL>".to_owned();
    };
    let Some(host) = parsed.host_str().filter(|host| !host.is_empty()) else {
        return "<invalid proxy URL>".to_owned();
    };
    let user = if parsed.username().is_empty() && parsed.password().is_none() {
        ""
    } else {
        "redacted@"
    };
    match parsed.port() {
        Some(port) => format!("{}://{user}{host}:{port}", parsed.scheme()),
        None => format!("{}://{user}{host}", parsed.scheme()),
    }
}

fn build_client(setting: &ProxySetting) -> reqwest::Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .redirect(crate::redirect::policy())
        .connect_timeout(CONNECT_TIMEOUT)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .tcp_keepalive(TCP_KEEPALIVE);
    match setting {
        ProxySetting::Inherit => builder.build(),
        ProxySetting::Direct => builder.no_proxy().build(),
        ProxySetting::Proxy(url) => builder.proxy(reqwest::Proxy::all(url.as_str())?).build(),
    }
}

/// HTTP clients by proxy URL, built on first use and then shared.
pub(crate) struct Clients {
    global_proxy_url: String,
    /// Which executor the log lines name.
    provider: &'static str,
    clients: Mutex<HashMap<String, reqwest::Client>>,
}

impl Clients {
    /// Clients that use `global_proxy_url` when a credential names none.
    pub(crate) fn new(global_proxy_url: impl Into<String>) -> Self {
        Self {
            global_proxy_url: global_proxy_url.into().trim().to_owned(),
            provider: "codex",
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// The same clients, with log lines that name `provider`.
    pub(crate) fn for_provider(mut self, provider: &'static str) -> Self {
        self.provider = provider;
        self
    }

    /// The proxy setting a credential with `proxy_url` goes through: its
    /// own, trimmed, or else the global one.
    pub(crate) fn effective_proxy<'a>(&'a self, proxy_url: &'a str) -> &'a str {
        let proxy_url = proxy_url.trim();
        if proxy_url.is_empty() {
            self.global_proxy_url.as_str()
        } else {
            proxy_url
        }
    }

    /// The client for a credential with `proxy_url`, or for the global proxy
    /// when it is empty.
    pub(crate) fn get(&self, proxy_url: &str) -> reqwest::Client {
        let effective = self.effective_proxy(proxy_url);
        if let Some(client) = self.cached(effective) {
            return client;
        }
        let client = match parse_proxy(effective)
            .and_then(|setting| build_client(&setting).map_err(|e| error_chain(&e)))
        {
            Ok(client) => client,
            Err(error) => {
                tracing::error!(
                    "{}: proxy {} can't be used: {error}",
                    self.provider,
                    redact_proxy_url(effective)
                );
                if effective == self.global_proxy_url {
                    self.inherit()
                } else {
                    self.get_global()
                }
            }
        };
        self.store(effective, client)
    }

    fn get_global(&self) -> reqwest::Client {
        let global = self.global_proxy_url.as_str();
        if let Some(client) = self.cached(global) {
            return client;
        }
        let client = parse_proxy(global)
            .and_then(|setting| build_client(&setting).map_err(|e| error_chain(&e)))
            .unwrap_or_else(|_| self.inherit());
        self.store(global, client)
    }

    fn inherit(&self) -> reqwest::Client {
        const KEY: &str = "\0inherit";
        if let Some(client) = self.cached(KEY) {
            return client;
        }
        let client = build_client(&ProxySetting::Inherit).unwrap_or_else(|error| {
            tracing::error!(
                "{}: HTTP client setup failed: {}",
                self.provider,
                error_chain(&error)
            );
            // As `Client::new` does, which follows every redirect.
            reqwest::Client::builder()
                .redirect(crate::redirect::policy())
                .build()
                .expect("an HTTP client without settings")
        });
        self.store(KEY, client)
    }

    fn cached(&self, key: &str) -> Option<reqwest::Client> {
        self.clients
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    fn store(&self, key: &str, client: reqwest::Client) -> reqwest::Client {
        self.clients
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(key.to_owned())
            .or_insert(client)
            .clone()
    }
}

/// An error and its sources, joined with `: `, as Go prints wrapped errors.
pub(crate) fn error_chain(error: &dyn StdError) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.ends_with(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// Why a body couldn't be read.
#[derive(Debug)]
pub(crate) enum ReadError {
    /// The connection failed.
    Http(reqwest::Error),
    /// The body was longer than the limit.
    TooLarge(usize),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(error) => f.write_str(&error_chain(error)),
            Self::TooLarge(limit) => write!(f, "response body is larger than {limit} bytes"),
        }
    }
}

impl std::error::Error for ReadError {}

/// Reads a whole body, failing past `limit` bytes.
pub(crate) async fn read_body(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ReadError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(ReadError::Http)? {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(ReadError::TooLarge(limit));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Reads at most `limit` bytes of a body and drops the rest, for error
/// bodies, where the start is what matters. When the connection fails, what
/// was read comes with the error, as Go's `io.ReadAll` gives it.
pub(crate) async fn read_body_prefix(
    mut response: reqwest::Response,
    limit: usize,
) -> (Vec<u8>, Option<reqwest::Error>) {
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let room = limit.saturating_sub(body.len());
                body.extend_from_slice(chunk.get(..room.min(chunk.len())).unwrap_or_default());
                if body.len() >= limit {
                    return (body, None);
                }
            }
            Ok(None) => return (body, None),
            Err(error) => return (body, Some(error.without_url())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clients_stay_within_the_origin() {
        let client = Clients::new("direct").get("");
        assert!(!crate::redirect::tests::crosses_origins(&client).await);
    }

    #[test]
    fn parses_proxy_settings() {
        assert_eq!(parse_proxy("  "), Ok(ProxySetting::Inherit));
        assert_eq!(parse_proxy("DIRECT"), Ok(ProxySetting::Direct));
        assert_eq!(parse_proxy(" none "), Ok(ProxySetting::Direct));
        assert_eq!(
            parse_proxy(" http://user:pass@proxy:8080 "),
            Ok(ProxySetting::Proxy("http://user:pass@proxy:8080".into()))
        );
        assert!(matches!(
            parse_proxy("https://proxy"),
            Ok(ProxySetting::Proxy(_))
        ));
        assert_eq!(
            parse_proxy("ftp://proxy").unwrap_err(),
            "unsupported proxy scheme: ftp"
        );
        assert!(parse_proxy("socks5://proxy:1080").is_err());
        assert!(parse_proxy("proxy:8080").is_err());
        assert!(parse_proxy("::").is_err());
    }

    #[test]
    fn redacts_proxy_urls() {
        assert_eq!(redact_proxy_url(""), "");
        assert_eq!(
            redact_proxy_url("http://user:secret@proxy:8080/x"),
            "http://redacted@proxy:8080"
        );
        assert_eq!(redact_proxy_url("http://proxy"), "http://proxy");
        assert_eq!(redact_proxy_url("nope"), "<invalid proxy URL>");
    }

    #[test]
    fn shares_clients_per_proxy() {
        let clients = Clients::new("http://global.invalid:8080");
        clients.get("");
        clients.get("direct");
        clients.get("ftp://bad");
        let keys: Vec<String> = {
            let map = clients.clients.lock().unwrap();
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();
            keys
        };
        assert_eq!(keys, ["direct", "ftp://bad", "http://global.invalid:8080"]);
    }
}
