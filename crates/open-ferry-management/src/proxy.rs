// Ported from CLIProxyAPI internal/api/handlers/management/api_tools.go
// (apiCallTransport, directAPICallTransport, resolveAPIKeyConfig,
// proxyURLFromAPIKeyConfig, resolveOpenAICompatAPIKeyProxyURL,
// buildProxyTransport) and sdk/proxyutil/proxy.go (Parse,
// BuildHTTPTransport, NewDirectTransport) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Which proxy an `api-call` goes through, and the HTTP clients that send
//! it.
//!
//! The request's `proxy_url` wins when given. Otherwise the first usable
//! of the credential's proxy, the proxy of the config entry its API key
//! comes from, and the config's `proxy-url`; else none. `direct` or `none`
//! means no proxy; a value that doesn't parse is passed over. Proxies from
//! the environment are never used.
//!
//! Deviations from upstream:
//! - SOCKS5 proxies are accepted as upstream accepts them, but a call
//!   through one fails with a 502: this port's HTTP client has no SOCKS
//!   support.
//! - Upstream builds a new connection pool for every call; this port keeps
//!   a client per proxy, at most 16, and starts over when that is reached.
//!   Idle connections close after 90 seconds, as upstream's would.
//! - The config has only `claude-api-key`, `codex-api-key` and
//!   `openai-compatibility` lists so far, so a key of another provider never
//!   finds a proxy in the config.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;

use crate::go::equal_fold;
use crate::go_url;

/// How long connecting may take (Go's default dialer timeout).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The TCP keep-alive interval (Go's default dialer).
const TCP_KEEPALIVE: Duration = Duration::from_secs(30);

/// How long an idle connection is kept (Go's `DefaultTransport`).
const POOL_IDLE: Duration = Duration::from_secs(90);

/// The most clients kept at once.
const MAX_CLIENTS: usize = 16;

/// A proxy setting, as upstream's `proxyutil.Parse` reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Setting {
    /// Empty: nothing configured.
    Inherit,
    /// `direct` or `none`: no proxy.
    Direct,
    /// A proxy URL, trimmed.
    Proxy(String),
}

/// Reads a proxy setting; `None` for one that is malformed or of a scheme
/// other than `socks5`, `socks5h`, `http` and `https` (upstream's
/// `proxyutil.Parse`).
pub(crate) fn parse(raw: &str) -> Option<Setting> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some(Setting::Inherit);
    }
    if equal_fold(trimmed, "direct") || equal_fold(trimmed, "none") {
        return Some(Setting::Direct);
    }
    let url = go_url::parse(trimmed.as_bytes())?;
    if url.scheme.is_empty() || url.host.is_empty() {
        return None;
    }
    matches!(url.scheme.as_str(), "socks5" | "socks5h" | "http" | "https")
        .then(|| Setting::Proxy(trimmed.to_owned()))
}

/// How an `api-call` connects: directly or through a proxy URL.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Route {
    Direct,
    Proxy(String),
}

impl Route {
    /// Whether requests to `scheme` URLs go to the proxy as they are, not
    /// through a tunnel: an HTTP or HTTPS proxy and a plain `http` URL.
    pub(crate) fn forwards(&self, scheme: &str) -> bool {
        match self {
            Self::Direct => false,
            Self::Proxy(url) => {
                scheme == "http"
                    && go_url::parse(url.as_bytes())
                        .is_some_and(|url| matches!(url.scheme.as_str(), "http" | "https"))
            }
        }
    }
}

/// The route a proxy setting gives, or `None` for one that gives none
/// (upstream's `buildProxyTransport`).
fn route(raw: &str) -> Option<Route> {
    match parse(raw)? {
        Setting::Inherit => None,
        Setting::Direct => Some(Route::Direct),
        Setting::Proxy(url) => Some(Route::Proxy(url)),
    }
}

/// The route for an `api-call` (upstream's `apiCallTransport`).
pub(crate) fn api_call_route(config: &Config, auth: Option<&Auth>, request_proxy: &str) -> Route {
    let request_proxy = request_proxy.trim();
    if !request_proxy.is_empty() {
        return route(request_proxy).unwrap_or(Route::Direct);
    }
    let mut candidates = Vec::new();
    if let Some(auth) = auth {
        candidates.push(auth.proxy_url.trim().to_owned());
        candidates.push(proxy_url_from_api_key_config(config, auth));
    }
    candidates.push(config.proxy_url.trim().to_owned());
    candidates
        .iter()
        .filter(|candidate| !candidate.is_empty())
        .find_map(|candidate| route(candidate))
        .unwrap_or(Route::Direct)
}

/// A config entry for an API key.
trait ApiKeyEntry {
    fn api_key(&self) -> &str;
    fn base_url(&self) -> &str;
    fn proxy_url(&self) -> &str;
}

impl ApiKeyEntry for open_ferry_core::config::ClaudeKey {
    fn api_key(&self) -> &str {
        &self.api_key
    }
    fn base_url(&self) -> &str {
        &self.base_url
    }
    fn proxy_url(&self) -> &str {
        &self.proxy_url
    }
}

impl ApiKeyEntry for open_ferry_core::config::CodexKey {
    fn api_key(&self) -> &str {
        &self.api_key
    }
    fn base_url(&self) -> &str {
        &self.base_url
    }
    fn proxy_url(&self) -> &str {
        &self.proxy_url
    }
}

/// The config entry an API-key credential comes from: the one with its key
/// and base URL, or with its key and no base URL, or, for a credential
/// without a key, its base URL; else the first with its key (upstream's
/// `resolveAPIKeyConfig`). Keys and URLs compare trimmed and regardless of
/// case.
fn resolve_api_key_config<'a, T: ApiKeyEntry>(entries: &'a [T], auth: &Auth) -> Option<&'a T> {
    let attribute = |key| auth.attribute(key).unwrap_or_default().trim();
    let (key, base) = (attribute("api_key"), attribute("base_url"));
    for entry in entries {
        let entry_key = entry.api_key().trim();
        let entry_base = entry.base_url().trim();
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
        .find(|entry| equal_fold(entry.api_key().trim(), key))
}

/// The proxy of the config entry an API-key credential comes from, or
/// empty (upstream's `proxyURLFromAPIKeyConfig`).
fn proxy_url_from_api_key_config(config: &Config, auth: &Auth) -> String {
    let Some((kind, _)) = auth.account_info() else {
        return String::new();
    };
    if !equal_fold(kind.trim(), "api_key") {
        return String::new();
    }
    let compat_name = auth.attribute("compat_name").unwrap_or_default().trim();
    let provider = auth.provider.trim();
    if !compat_name.is_empty() || equal_fold(provider, "openai-compatibility") {
        let provider_key = auth.attribute("provider_key").unwrap_or_default().trim();
        return resolve_openai_compat_api_key_proxy_url(config, auth, provider_key, compat_name);
    }
    let proxy = match open_ferry_translate::go::to_lower(provider).as_str() {
        "claude" => {
            resolve_api_key_config(&config.claude_api_key, auth).map(ApiKeyEntry::proxy_url)
        }
        "codex" => resolve_api_key_config(&config.codex_api_key, auth).map(ApiKeyEntry::proxy_url),
        _ => None,
    };
    proxy.unwrap_or_default().trim().to_owned()
}

/// The proxy of an OpenAI-compatible credential's API key: that of the
/// matching key in the first enabled provider named by the credential's
/// `compat_name`, `provider_key` or provider, or empty (upstream's
/// `resolveOpenAICompatAPIKeyProxyURL`).
fn resolve_openai_compat_api_key_proxy_url(
    config: &Config,
    auth: &Auth,
    provider_key: &str,
    compat_name: &str,
) -> String {
    let api_key = auth.attribute("api_key").unwrap_or_default().trim();
    if api_key.is_empty() {
        return String::new();
    }
    let candidates: Vec<&str> = [compat_name, provider_key, auth.provider.trim()]
        .into_iter()
        .filter(|candidate| !candidate.is_empty())
        .collect();
    for compat in &config.openai_compatibility {
        if compat.disabled {
            continue;
        }
        if candidates
            .iter()
            .any(|candidate| equal_fold(candidate, &compat.name))
        {
            return compat
                .api_key_entries
                .iter()
                .find(|entry| equal_fold(entry.api_key.trim(), api_key))
                .map(|entry| entry.proxy_url.trim().to_owned())
                .unwrap_or_default();
        }
    }
    String::new()
}

/// The HTTP clients for `api-call`, one per route and HTTP version policy.
#[derive(Default)]
pub(crate) struct Clients {
    clients: Mutex<HashMap<(Route, bool), reqwest::Client>>,
}

impl Clients {
    /// The client for `route`; `http1_only` keeps it to HTTP/1.1, so that a
    /// `Host` header the caller sets is sent as one.
    pub(crate) fn get(
        &self,
        route: &Route,
        http1_only: bool,
    ) -> Result<reqwest::Client, reqwest::Error> {
        let key = (route.clone(), http1_only);
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(client) = clients.get(&key) {
            return Ok(client.clone());
        }
        let client = build(route, http1_only)?;
        if clients.len() >= MAX_CLIENTS {
            clients.clear();
        }
        clients.insert(key, client.clone());
        Ok(client)
    }
}

/// A client as upstream's transports behave: no redirects followed by
/// itself, no `Referer`, no user agent of its own, no decompression, and
/// header names written as Go writes them.
fn build(route: &Route, http1_only: bool) -> Result<reqwest::Client, reqwest::Error> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_keepalive(TCP_KEEPALIVE)
        .pool_idle_timeout(POOL_IDLE)
        .http1_title_case_headers();
    builder = match route {
        Route::Direct => builder.no_proxy(),
        Route::Proxy(url) => builder.proxy(reqwest::Proxy::all(url.as_str())?),
    };
    if http1_only {
        builder = builder.http1_only();
    }
    builder.build()
}

impl std::fmt::Debug for Clients {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clients").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use open_ferry_core::config::{
        ClaudeKey, CodexKey, OpenAiCompatibility, OpenAiCompatibilityApiKey,
    };

    use super::*;

    #[test]
    fn settings_parse_as_upstream() {
        assert_eq!(parse("  "), Some(Setting::Inherit));
        assert_eq!(parse(" DIRECT "), Some(Setting::Direct));
        assert_eq!(parse("None"), Some(Setting::Direct));
        assert_eq!(
            parse(" http://u:p@proxy:8080 "),
            Some(Setting::Proxy("http://u:p@proxy:8080".into()))
        );
        assert_eq!(
            parse("SOCKS5H://proxy:1080"),
            Some(Setting::Proxy("SOCKS5H://proxy:1080".into()))
        );
        assert_eq!(parse("ftp://proxy"), None);
        assert_eq!(parse("proxy:8080"), None);
        assert_eq!(parse("http://"), None);
        assert_eq!(parse("http://proxy:x"), None);
        assert_eq!(parse("://proxy"), None);
    }

    fn auth(provider: &str, attributes: &[(&str, &str)]) -> Auth {
        Auth {
            provider: provider.into(),
            attributes: attributes
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            ..Auth::default()
        }
    }

    fn claude(key: &str, base: &str, proxy: &str) -> ClaudeKey {
        ClaudeKey {
            api_key: key.into(),
            base_url: base.into(),
            proxy_url: proxy.into(),
            ..ClaudeKey::default()
        }
    }

    #[test]
    fn routes_follow_upstream_priority() {
        let mut config = Config::default();
        config.proxy_url = "http://global:1".into();
        let mut credential = auth("claude", &[]);
        assert_eq!(
            api_call_route(&config, None, " http://request:1 "),
            Route::Proxy("http://request:1".into())
        );
        assert_eq!(api_call_route(&config, None, "direct"), Route::Direct);
        assert_eq!(
            api_call_route(&config, Some(&credential), ""),
            Route::Proxy("http://global:1".into())
        );
        credential.proxy_url = " http://credential:1 ".into();
        assert_eq!(
            api_call_route(&config, Some(&credential), ""),
            Route::Proxy("http://credential:1".into())
        );
        // A setting that doesn't parse is passed over; "none" stops the
        // search.
        credential.proxy_url = "ftp://bad".into();
        assert_eq!(
            api_call_route(&config, Some(&credential), ""),
            Route::Proxy("http://global:1".into())
        );
        credential.proxy_url = "none".into();
        assert_eq!(
            api_call_route(&config, Some(&credential), ""),
            Route::Direct
        );
        config.proxy_url = String::new();
        assert_eq!(api_call_route(&config, None, ""), Route::Direct);
    }

    #[test]
    fn api_key_entries_resolve_as_upstream() {
        let mut config = Config::default();
        config.claude_api_key = vec![
            claude("k1", "https://a", "http://first:1"),
            claude("K1", "", "http://second:1"),
            claude("k2", "https://b", "http://third:1"),
        ];
        config.codex_api_key = vec![CodexKey {
            api_key: "c".into(),
            proxy_url: " http://codex:1 ".into(),
            ..CodexKey::default()
        }];
        let proxy = |provider, attrs: &[(&str, &str)]| {
            proxy_url_from_api_key_config(&config, &auth(provider, attrs))
        };
        assert_eq!(
            proxy("claude", &[("api_key", "k1"), ("base_url", "HTTPS://A")]),
            "http://first:1"
        );
        assert_eq!(
            proxy("claude", &[("api_key", "k1"), ("base_url", "https://z")]),
            "http://first:1"
        );
        assert_eq!(proxy("claude", &[("api_key", "k1")]), "http://second:1");
        assert_eq!(proxy("claude", &[("api_key", "kx")]), "");
        assert_eq!(proxy("codex", &[("api_key", "c")]), "http://codex:1");
        assert_eq!(proxy("gemini", &[("api_key", "k1")]), "");
        assert_eq!(
            proxy("claude", &[("api_key", "k1"), ("compat_name", "x")]),
            ""
        );
    }

    // The openai-compatibility case of
    // TestAPICallTransportAPIKeyAuthFallsBackToConfigProxyURL, with the
    // lookup's other rules.
    #[test]
    fn openai_compatible_keys_use_their_entry() {
        let entry = |key: &str, proxy: &str| OpenAiCompatibilityApiKey {
            api_key: key.into(),
            proxy_url: proxy.into(),
            ..OpenAiCompatibilityApiKey::default()
        };
        let mut config = Config::default();
        config.openai_compatibility = vec![
            OpenAiCompatibility {
                name: "off".into(),
                disabled: true,
                api_key_entries: vec![entry("compat-key", "http://disabled:1")],
                ..OpenAiCompatibility::default()
            },
            OpenAiCompatibility {
                name: "bohe".into(),
                base_url: "https://bohe.example.com".into(),
                api_key_entries: vec![entry(
                    "compat-key",
                    " http://compat-proxy.example.com:8080 ",
                )],
                ..OpenAiCompatibility::default()
            },
            OpenAiCompatibility {
                name: "Off".into(),
                api_key_entries: vec![entry("compat-key", "http://second-off:1")],
                ..OpenAiCompatibility::default()
            },
        ];
        let proxy = |provider, attrs: &[(&str, &str)]| {
            proxy_url_from_api_key_config(&config, &auth(provider, attrs))
        };
        assert_eq!(
            proxy(
                "bohe",
                &[
                    ("api_key", "compat-key"),
                    ("compat_name", "bohe"),
                    ("provider_key", "bohe"),
                ]
            ),
            "http://compat-proxy.example.com:8080"
        );
        assert_eq!(
            proxy(
                "openai-compatibility",
                &[("api_key", "COMPAT-KEY"), ("provider_key", "BOHE")]
            ),
            "http://compat-proxy.example.com:8080",
            "the provider key names the provider; keys match in any case"
        );
        assert_eq!(
            proxy("x", &[("api_key", "other"), ("compat_name", "bohe")]),
            "",
            "the first provider named decides"
        );
        assert_eq!(
            proxy("x", &[("api_key", "compat-key"), ("compat_name", "off")]),
            "http://second-off:1",
            "disabled providers are passed over"
        );
        assert_eq!(proxy("x", &[("compat_name", "bohe")]), "", "no key");
    }

    #[test]
    fn only_plain_http_through_an_http_proxy_is_forwarded() {
        let http = Route::Proxy("http://p:1".into());
        assert!(http.forwards("http"));
        assert!(!http.forwards("https"));
        assert!(!Route::Proxy("socks5://p:1".into()).forwards("http"));
        assert!(!Route::Direct.forwards("http"));
    }
}
