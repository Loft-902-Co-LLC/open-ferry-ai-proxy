// Ported from CLIProxyAPI internal/runtime/executor/codex_websockets_connection.go
// (dialCodexWebsocket, newProxyAwareWebsocketDialer, executionProxyURL)
// (v8.0.10, MIT), with the client handshake of gorilla/websocket's client.go
// (Dialer.DialContext, hostPortNoPort), util.go (tokenListContainsValue) and
// proxy.go (httpProxyDialer, proxy_FromURL) (v1.5.3, BSD-2-Clause), and the
// environment's proxy as Go's vendor/golang.org/x/net/http/httpproxy/proxy.go
// (FromEnvironment, proxyForURL, parseProxy, useProxy, init) reads it
// (go1.26, BSD-3-Clause).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/gorilla/websocket
// https://github.com/golang/go

//! Connecting to Codex's Responses WebSocket: TCP, an HTTP proxy's
//! `CONNECT`, TLS and the WebSocket handshake, all within 30 seconds.
//!
//! The proxy is the credential's `proxy_url`, or else the config's
//! `proxy-url`: `direct` or `none` means no proxy, and an `http` URL a proxy
//! reached with `CONNECT`. An empty setting, or one that doesn't parse or
//! names another scheme, falls back to the environment's proxy as Go's
//! `ProxyFromEnvironment` reads it: `HTTPS_PROXY` for `wss`, `HTTP_PROXY` for
//! `ws` (refused in a CGI environment), except for loopback and the hosts
//! `NO_PROXY` names. An `https` proxy fails as it does upstream (gorilla only
//! knows `http` proxies).
//!
//! A refused handshake gives its status and up to 1 KiB of its body, which
//! the caller reads as upstream reads an `ErrBadHandshake`.
//!
//! Deviations from upstream:
//! - A SOCKS5 proxy, set or from the environment, is refused with a 502, as
//!   `api-call` refuses it; upstream dials through it.
//! - `permessage-deflate` isn't offered (upstream offers it but never
//!   compresses what it sends), and a server that picks an extension anyway
//!   is refused.
//! - A message or frame may be at most 50 MiB, and the response head 64 KiB;
//!   gorilla has no limits.
//! - `CONNECT` says `User-Agent: open-ferry/<version>`, and a refused one's
//!   error gives the proxy's status as well as its reason.
//! - The environment's proxy is read at each connection; Go reads it once.
//! - Addresses are tried one after another, without Go's Happy Eyeballs.
//! - The `Host` header leaves out a default port the URL names, and an
//!   unusable URL's error doesn't quote it.
//! - TLS goes through rustls with the platform's verifier, without ALPN, as
//!   gorilla sends none.

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use open_ferry_core::exec::{ErrorKind, ExecError, TransportFault};
use percent_encoding::percent_decode_str;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use rustls_platform_verifier::BuilderVerifierExt;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout_at};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};

use crate::codex::client::{USER_AGENT, redact_proxy_url};

/// How long connecting may take, from TCP to the handshake's answer
/// (`codexResponsesWebsocketHandshakeTO` and the net dialer's timeout).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// TCP keep-alive, as upstream's dialer sets it.
const TCP_KEEPALIVE: Duration = Duration::from_secs(30);
/// The largest response head read.
const MAX_HEAD: usize = 64 * 1024;
/// The most response headers read.
const MAX_HEADERS: usize = 128;
/// How much of a refused handshake's body is kept, as gorilla keeps it.
const MAX_ERROR_BODY: usize = 1024;
/// The most raw bytes read for a refused handshake's chunked body.
const MAX_ERROR_RAW: usize = 16 * 1024;
/// The largest message or frame read.
pub(super) const MAX_MESSAGE: usize = 50 * 1024 * 1024;

/// A byte stream the WebSocket runs over: TCP, or TLS over TCP.
pub(super) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// A connected WebSocket.
pub(super) type WsStream = WebSocketStream<Box<dyn Io>>;

/// A connection made, with the handshake's response headers.
pub(super) struct Dialed {
    pub(super) stream: WsStream,
    pub(super) headers: HeaderMap,
}

/// Why connecting failed.
#[derive(Debug)]
pub(super) enum DialError {
    /// Codex answered the handshake but refused it (gorilla's
    /// `ErrBadHandshake`), with the start of its body.
    Handshake { status: u16, body: Vec<u8> },
    /// Anything else.
    Failed(ExecError),
}

impl From<ExecError> for DialError {
    fn from(error: ExecError) -> Self {
        Self::Failed(error)
    }
}

/// Connects to `url` (`ws` or `wss`) through `proxy`, sending `headers` with
/// the handshake.
pub(super) fn dial(
    proxy: &str,
    url: &str,
    headers: &HeaderMap,
) -> impl Future<Output = Result<Dialed, DialError>> + Send + use<> {
    dial_with_env(proxy, url, headers, &|name| std::env::var(name).ok())
}

/// [`dial`], reading the environment through `env`.
pub(super) fn dial_with_env(
    proxy: &str,
    url: &str,
    headers: &HeaderMap,
    env: &dyn Fn(&str) -> Option<String>,
) -> impl Future<Output = Result<Dialed, DialError>> + Send + use<> {
    let plan = plan(proxy, url, headers, env);
    async move {
        let plan = plan?;
        connect(plan, Instant::now() + HANDSHAKE_TIMEOUT).await
    }
}

/// Where to connect, and what to send there.
struct Plan {
    target: Target,
    route: Route,
    request: Vec<u8>,
    key: String,
}

/// What to connect to, from the URL.
struct Target {
    /// `wss`.
    tls: bool,
    /// The host, without brackets, for DNS and TLS.
    host: String,
    port: u16,
    /// `host:port`, IPv6 in brackets, for `CONNECT`.
    authority: String,
}

/// How to reach the target.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Route {
    /// Straight to it.
    Direct,
    /// Through an HTTP proxy's `CONNECT`.
    Connect(Proxy),
}

/// An HTTP proxy.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Proxy {
    /// The host, without brackets.
    pub(super) host: String,
    pub(super) port: u16,
    /// The `Proxy-Authorization` value, when the URL has a password.
    authorization: Option<String>,
}

/// Everything decided before connecting: the target, the handshake request
/// and the route (in gorilla's order).
fn plan(
    proxy: &str,
    url: &str,
    headers: &HeaderMap,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Plan, ExecError> {
    let parsed = url::Url::parse(url).map_err(|error| {
        failed(format!(
            "codex websockets executor: invalid websocket URL: {error}"
        ))
    })?;
    let tls = match parsed.scheme() {
        "ws" => false,
        "wss" => true,
        _ => return Err(malformed_url()),
    };
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(malformed_url());
    }
    let host = host_of(&parsed).ok_or_else(malformed_url)?;
    let host_str = parsed.host_str().unwrap_or_default();
    let port = parsed
        .port_or_known_default()
        .unwrap_or(if tls { 443 } else { 80 });
    let host_header = match parsed.port() {
        Some(port) => format!("{host_str}:{port}"),
        None => host_str.to_owned(),
    };
    let mut uri = parsed.path().to_owned();
    if uri.is_empty() {
        uri.push('/');
    }
    if let Some(query) = parsed.query() {
        uri.push('?');
        uri.push_str(query);
    }
    let target = Target {
        tls,
        authority: format!("{host_str}:{port}"),
        host,
        port,
    };
    let key = generate_key();
    let request = handshake_request(&uri, &host_header, headers, &key)?;
    let route = route(proxy, tls, &target.host, target.port, env)?;
    Ok(Plan {
        target,
        route,
        request,
        key,
    })
}

/// A URL's host without brackets.
fn host_of(url: &url::Url) -> Option<String> {
    Some(match url.host()? {
        url::Host::Domain(domain) => domain.to_owned(),
        url::Host::Ipv4(addr) => addr.to_string(),
        url::Host::Ipv6(addr) => addr.to_string(),
    })
}

/// gorilla's `errMalformedURL`.
fn malformed_url() -> ExecError {
    failed("malformed ws or wss URL")
}

/// The handshake request for `uri`, with `headers` as gorilla merges them:
/// a `Host` header replaces the host, and the handshake's own headers may not
/// be given.
fn handshake_request(
    uri: &str,
    host: &str,
    headers: &HeaderMap,
    key: &str,
) -> Result<Vec<u8>, ExecError> {
    let mut lines: Vec<(String, &[u8])> = vec![
        ("Upgrade".to_owned(), b"websocket"),
        ("Connection".to_owned(), b"Upgrade"),
        ("Sec-WebSocket-Key".to_owned(), key.as_bytes()),
        ("Sec-WebSocket-Version".to_owned(), b"13"),
    ];
    for (name, value) in headers {
        match name.as_str() {
            "host" | "user-agent" | "content-length" | "transfer-encoding" | "trailer" => {}
            "upgrade"
            | "connection"
            | "sec-websocket-key"
            | "sec-websocket-version"
            | "sec-websocket-extensions" => {
                return Err(failed(format!(
                    "websocket: duplicate header not allowed: {}",
                    canonical_name(name)
                )));
            }
            "sec-websocket-protocol" => {
                lines.push(("Sec-WebSocket-Protocol".to_owned(), value.as_bytes()));
            }
            _ => lines.push((canonical_name(name), value.as_bytes())),
        }
    }
    lines.sort_by(|a, b| a.0.cmp(&b.0));

    let mut request = Vec::with_capacity(512);
    request.extend_from_slice(b"GET ");
    request.extend_from_slice(uri.as_bytes());
    request.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    match headers.get(header::HOST) {
        Some(value) => request.extend_from_slice(value.as_bytes()),
        None => request.extend_from_slice(host.as_bytes()),
    }
    request.extend_from_slice(b"\r\n");
    let user_agent = headers
        .get(header::USER_AGENT)
        .map_or(USER_AGENT.as_bytes(), HeaderValue::as_bytes);
    if !user_agent.is_empty() {
        request.extend_from_slice(b"User-Agent: ");
        request.extend_from_slice(user_agent);
        request.extend_from_slice(b"\r\n");
    }
    for (name, value) in lines {
        request.extend_from_slice(name.as_bytes());
        request.extend_from_slice(b": ");
        request.extend_from_slice(value);
        request.extend_from_slice(b"\r\n");
    }
    request.extend_from_slice(b"\r\n");
    Ok(request)
}

/// A header name as Go's `CanonicalMIMEHeaderKey` writes it.
fn canonical_name(name: &HeaderName) -> String {
    let mut upper = true;
    name.as_str()
        .chars()
        .map(|c| {
            let out = if upper {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            upper = c == '-';
            out
        })
        .collect()
}

/// How to reach `host:port` with the proxy setting `proxy`
/// (`newProxyAwareWebsocketDialer`).
pub(super) fn route(
    proxy: &str,
    tls: bool,
    host: &str,
    port: u16,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Route, ExecError> {
    let proxy = proxy.trim();
    if proxy.is_empty() {
        return env_route(tls, host, port, env);
    }
    if proxy.eq_ignore_ascii_case("direct") || proxy.eq_ignore_ascii_case("none") {
        return Ok(Route::Direct);
    }
    match url::Url::parse(proxy) {
        Ok(url) if url.host_str().is_some_and(|host| !host.is_empty()) => match url.scheme() {
            "http" | "https" | "socks5" | "socks5h" => through(&url, proxy),
            scheme => {
                tracing::error!(
                    "codex websockets executor: unsupported proxy scheme: {scheme} ({}); using the environment's proxy",
                    redact_proxy_url(proxy)
                );
                env_route(tls, host, port, env)
            }
        },
        _ => {
            tracing::error!(
                "codex websockets executor: invalid proxy URL {}; using the environment's proxy",
                redact_proxy_url(proxy)
            );
            env_route(tls, host, port, env)
        }
    }
}

/// The route through the proxy at `url` (`raw` as given), as gorilla's
/// `proxy_FromURL` takes it.
fn through(url: &url::Url, raw: &str) -> Result<Route, ExecError> {
    match url.scheme() {
        "http" => {
            let host = host_of(url).ok_or_else(|| {
                failed(format!(
                    "codex websockets executor: proxy {} has no host",
                    redact_proxy_url(raw)
                ))
            })?;
            let authorization = url.password().map(|password| {
                let mut credential: Vec<u8> = percent_decode_str(url.username()).collect();
                credential.push(b':');
                credential.extend(percent_decode_str(password));
                format!("Basic {}", STANDARD.encode(credential))
            });
            Ok(Route::Connect(Proxy {
                host,
                port: url.port_or_known_default().unwrap_or(80),
                authorization,
            }))
        }
        "socks5" | "socks5h" => Err(ExecError::upstream(
            502,
            format!(
                "codex websockets executor: SOCKS5 proxy {} isn't supported",
                redact_proxy_url(raw)
            ),
        )),
        scheme => Err(failed(format!("proxy: unknown scheme: {scheme}"))),
    }
}

/// The environment's proxy for `host:port` (`ProxyFromEnvironment`).
fn env_route(
    tls: bool,
    host: &str,
    port: u16,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Route, ExecError> {
    let any = |names: [&str; 2]| {
        names
            .into_iter()
            .find_map(|name| env(name).filter(|value| !value.is_empty()))
    };
    let raw = if tls {
        any(["HTTPS_PROXY", "https_proxy"])
    } else {
        any(["HTTP_PROXY", "http_proxy"])
    };
    let Some(url) = raw.as_deref().and_then(parse_env_proxy) else {
        return Ok(Route::Direct);
    };
    if !tls && env("REQUEST_METHOD").is_some_and(|value| !value.is_empty()) {
        return Err(failed(
            "refusing to use HTTP_PROXY value in CGI environment; see golang.org/s/cgihttpproxy",
        ));
    }
    let no_proxy = any(["NO_PROXY", "no_proxy"]).unwrap_or_default();
    if !use_proxy(host, port, &no_proxy) {
        return Ok(Route::Direct);
    }
    through(&url, raw.as_deref().unwrap_or_default())
}

/// An environment proxy setting as a URL (`parseProxy`): one without a
/// scheme or host is read as `http://` plus it.
fn parse_env_proxy(raw: &str) -> Option<url::Url> {
    let first = url::Url::parse(raw);
    let usable = first
        .as_ref()
        .is_ok_and(|url| url.host_str().is_some_and(|host| !host.is_empty()));
    if !usable && let Ok(retry) = url::Url::parse(&format!("http://{raw}")) {
        return Some(retry);
    }
    first.ok()
}

/// Whether `host:port` goes through the environment's proxy (`useProxy`):
/// not for `localhost`, loopback, or a host `no_proxy` names.
pub(super) fn use_proxy(host: &str, port: u16, no_proxy: &str) -> bool {
    if host == "localhost" {
        return false;
    }
    let ip = host.parse::<IpAddr>().ok().map(|ip| ip.to_canonical());
    if ip.is_some_and(|ip| ip.is_loopback()) {
        return false;
    }
    let host = host.trim().to_lowercase();
    let port = port.to_string();
    for entry in no_proxy.split(',') {
        let entry = entry.trim().to_lowercase();
        if entry.is_empty() {
            continue;
        }
        if entry == "*" {
            return false;
        }
        if let Some((network, bits)) = parse_cidr(&entry) {
            if ip.is_some_and(|ip| cidr_contains(network, bits, ip)) {
                return false;
            }
            continue;
        }
        let (entry_host, entry_port) = match split_host_port(&entry) {
            Some(("", _)) => continue,
            Some((entry_host, entry_port)) => (entry_host, entry_port),
            None => (entry.as_str(), ""),
        };
        let port_matches = entry_port.is_empty() || entry_port == port;
        if let Ok(entry_ip) = entry_host.parse::<IpAddr>() {
            if ip == Some(entry_ip.to_canonical()) && port_matches {
                return false;
            }
            continue;
        }
        if entry_host.is_empty() || ip.is_some() {
            continue;
        }
        let entry_host = if entry_host.starts_with("*.") {
            entry_host.get(1..).unwrap_or(entry_host)
        } else {
            entry_host
        };
        let (suffix, match_host) = match entry_host.strip_prefix('.') {
            Some(_) => (idna_ascii(entry_host), false),
            None => (idna_ascii(&format!(".{entry_host}")), true),
        };
        let exact = match_host && suffix.strip_prefix('.') == Some(host.as_str());
        if (host.ends_with(&suffix) || exact) && port_matches {
            return false;
        }
    }
    true
}

/// `ip/bits` as a network, when it is one (Go's `net.ParseCIDR`).
fn parse_cidr(entry: &str) -> Option<(IpAddr, u8)> {
    let (addr, bits) = entry.split_once('/')?;
    let addr = addr.parse::<IpAddr>().ok()?;
    if bits.is_empty() || !bits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let bits = bits.parse::<u8>().ok()?;
    let max = if addr.is_ipv4() { 32 } else { 128 };
    (bits <= max).then_some((addr, bits))
}

/// Whether the network `network/bits` holds `ip` (Go's `IPNet.Contains`).
fn cidr_contains(network: IpAddr, bits: u8, ip: IpAddr) -> bool {
    match (network, ip) {
        (IpAddr::V4(network), IpAddr::V4(ip)) => {
            let mask = u32::MAX
                .checked_shl(32u32.saturating_sub(u32::from(bits)))
                .unwrap_or(0);
            u32::from(network) & mask == u32::from(ip) & mask
        }
        (IpAddr::V6(network), IpAddr::V6(ip)) => {
            let mask = u128::MAX
                .checked_shl(128u32.saturating_sub(u32::from(bits)))
                .unwrap_or(0);
            u128::from(network) & mask == u128::from(ip) & mask
        }
        _ => false,
    }
}

/// Go's `net.SplitHostPort`, roughly: `host:port` or `[host]:port`.
fn split_host_port(text: &str) -> Option<(&str, &str)> {
    let (host, port) = text.rsplit_once(':')?;
    if let Some(inner) = host.strip_prefix('[') {
        let inner = inner.strip_suffix(']')?;
        return (!inner.contains(['[', ']'])).then_some((inner, port));
    }
    (!host.contains([':', '[', ']'])).then_some((host, port))
}

/// A domain (with a leading dot) in its ASCII form, when it has another.
fn idna_ascii(domain: &str) -> String {
    if domain.is_ascii() {
        return domain.to_owned();
    }
    let (dot, rest) = match domain.strip_prefix('.') {
        Some(rest) => (".", rest),
        None => ("", domain),
    };
    match url::Host::parse(rest) {
        Ok(url::Host::Domain(ascii)) => format!("{dot}{ascii}"),
        _ => domain.to_owned(),
    }
}

/// Connects as `plan` says, by `deadline`.
async fn connect(plan: Plan, deadline: Instant) -> Result<Dialed, DialError> {
    let Plan {
        target,
        route,
        request,
        key,
    } = plan;
    let tcp = match &route {
        Route::Direct => tcp_connect(&target.host, target.port, deadline).await?,
        Route::Connect(proxy) => {
            let mut tcp = tcp_connect(&proxy.host, proxy.port, deadline).await?;
            proxy_connect(
                &mut tcp,
                &target.authority,
                proxy.authorization.as_deref(),
                deadline,
            )
            .await?;
            tcp
        }
    };
    let mut io: Box<dyn Io> = if target.tls {
        Box::new(tls_connect(tcp, &target.host, deadline).await?)
    } else {
        Box::new(tcp)
    };
    let (head, rest) = within(deadline, "handshake", async {
        io.write_all(&request)
            .await
            .map_err(|error| io_error("write", &error))?;
        io.flush()
            .await
            .map_err(|error| io_error("write", &error))?;
        read_head(&mut io).await
    })
    .await?;

    let accept = derive_accept_key(key.as_bytes());
    let accepted = head.status == 101
        && token_list_contains(&head.headers, header::UPGRADE, "websocket")
        && token_list_contains(&head.headers, header::CONNECTION, "upgrade")
        && head
            .headers
            .get(header::SEC_WEBSOCKET_ACCEPT)
            .is_some_and(|value| value.as_bytes().trim_ascii() == accept.as_bytes());
    if !accepted {
        let body = read_error_body(&mut io, &head, rest, deadline).await;
        return Err(DialError::Handshake {
            status: head.status,
            body,
        });
    }
    if head
        .headers
        .get_all(header::SEC_WEBSOCKET_EXTENSIONS)
        .iter()
        .any(|value| !value.as_bytes().trim_ascii().is_empty())
    {
        return Err(failed(
            "codex websockets executor: the server chose a WebSocket extension that wasn't offered",
        )
        .into());
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let stream = WebSocketStream::from_partially_read(io, rest, Role::Client, Some(config)).await;
    Ok(Dialed {
        stream,
        headers: head.headers,
    })
}

/// Runs `step` until `deadline`, when it fails as Go's deadline does.
async fn within<T>(
    deadline: Instant,
    step: &str,
    future: impl Future<Output = Result<T, ExecError>>,
) -> Result<T, ExecError> {
    timeout_at(deadline, future).await.unwrap_or_else(|_| {
        Err(transient(format!(
            "codex websockets executor: {step}: i/o timeout"
        )))
    })
}

/// Connects to `host:port`, trying each of its addresses.
async fn tcp_connect(host: &str, port: u16, deadline: Instant) -> Result<TcpStream, ExecError> {
    within(deadline, "dial", async {
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
            .await
            .map_err(|error| transient(format!("dial tcp: lookup {host}: {error}")))?
            .collect();
        let mut last = None;
        for addr in addrs {
            match TcpStream::connect(addr).await {
                Ok(tcp) => {
                    // Both are best effort, as Go ignores their failures.
                    let _ = tcp.set_nodelay(true);
                    let keepalive = socket2::TcpKeepalive::new().with_time(TCP_KEEPALIVE);
                    let _ = socket2::SockRef::from(&tcp).set_tcp_keepalive(&keepalive);
                    return Ok(tcp);
                }
                Err(error) => last = Some(transient(format!("dial tcp {addr}: {error}"))),
            }
        }
        Err(last.unwrap_or_else(|| transient(format!("dial tcp: lookup {host}: no such host"))))
    })
    .await
}

/// Asks the proxy on `tcp` to `CONNECT` to `authority` (gorilla's
/// `httpProxyDialer`).
async fn proxy_connect(
    tcp: &mut TcpStream,
    authority: &str,
    authorization: Option<&str>,
    deadline: Instant,
) -> Result<(), ExecError> {
    let mut request = format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: {USER_AGENT}\r\n"
    );
    if let Some(authorization) = authorization {
        request.push_str("Proxy-Authorization: ");
        request.push_str(authorization);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    within(deadline, "proxy CONNECT", async {
        tcp.write_all(request.as_bytes())
            .await
            .map_err(|error| io_error("write", &error))?;
        // As gorilla, whatever follows the response head is dropped.
        let (head, _) = read_head(tcp).await?;
        if head.status != 200 {
            return Err(failed(format!(
                "codex websockets executor: proxy CONNECT failed: {} {}",
                head.status, head.reason
            )));
        }
        Ok(())
    })
    .await
}

/// The TLS client config, built once.
fn tls_config() -> Result<Arc<ClientConfig>, ExecError> {
    static CONFIG: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::aws_lc_rs::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .and_then(BuilderVerifierExt::with_platform_verifier)
            .map(|builder| Arc::new(builder.with_no_client_auth()))
            .map_err(|error| error.to_string())
        })
        .clone()
        .map_err(|error| failed(format!("codex websockets executor: tls: {error}")))
}

/// Shakes hands with TLS over `tcp` for `host`.
async fn tls_connect(
    tcp: TcpStream,
    host: &str,
    deadline: Instant,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, ExecError> {
    let name = ServerName::try_from(host.to_owned()).map_err(|error| {
        failed(format!(
            "codex websockets executor: tls: invalid server name: {error}"
        ))
    })?;
    let connector = TlsConnector::from(tls_config()?);
    within(deadline, "tls handshake", async {
        connector
            .connect(name, tcp)
            .await
            .map_err(|error| io_error("tls", &error))
    })
    .await
}

/// A response's status line and headers.
struct Head {
    status: u16,
    reason: String,
    headers: HeaderMap,
}

/// Reads a response head, returning it and the bytes read past it.
async fn read_head<R: AsyncRead + Unpin + ?Sized>(
    io: &mut R,
) -> Result<(Head, Vec<u8>), ExecError> {
    let mut buf = Vec::with_capacity(4096);
    loop {
        buf.reserve(4096);
        let read = io
            .read_buf(&mut buf)
            .await
            .map_err(|error| io_error("read", &error))?;
        if read == 0 {
            let text = if buf.is_empty() {
                "EOF"
            } else {
                "unexpected EOF"
            };
            return Err(
                ExecError::new(ErrorKind::Upstream, text).with_transport(TransportFault::Lifecycle)
            );
        }
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut response = httparse::Response::new(&mut headers);
        match response.parse(&buf) {
            Ok(httparse::Status::Complete(len)) => {
                let mut map = HeaderMap::new();
                for parsed in response.headers.iter() {
                    if let (Ok(name), Ok(value)) = (
                        HeaderName::from_bytes(parsed.name.as_bytes()),
                        HeaderValue::from_bytes(parsed.value),
                    ) {
                        map.append(name, value);
                    }
                }
                let head = Head {
                    status: response.code.unwrap_or_default(),
                    reason: response.reason.unwrap_or_default().to_owned(),
                    headers: map,
                };
                return Ok((head, buf.get(len..).unwrap_or_default().to_vec()));
            }
            Ok(httparse::Status::Partial) if buf.len() > MAX_HEAD => {
                return Err(failed(
                    "codex websockets executor: the response head is too large",
                ));
            }
            Ok(httparse::Status::Partial) => {}
            Err(error) => {
                return Err(failed(format!(
                    "codex websockets executor: malformed HTTP response: {error}"
                )));
            }
        }
    }
}

/// Whether a `name` header lists the token `value` (gorilla's
/// `tokenListContainsValue`).
fn token_list_contains(headers: &HeaderMap, name: HeaderName, value: &str) -> bool {
    headers.get_all(name).iter().any(|raw| {
        let mut rest = raw.as_bytes();
        loop {
            rest = skip_space(rest);
            let end = rest
                .iter()
                .position(|b| !is_token_byte(*b))
                .unwrap_or(rest.len());
            let Some((token, after)) = rest.split_at_checked(end) else {
                return false;
            };
            if token.is_empty() {
                return false;
            }
            let after = skip_space(after);
            match after.split_first() {
                Some((b',', next)) => {
                    if token.eq_ignore_ascii_case(value.as_bytes()) {
                        return true;
                    }
                    rest = next;
                }
                Some(_) => return false,
                None => return token.eq_ignore_ascii_case(value.as_bytes()),
            }
        }
    })
}

/// `bytes` without its leading spaces and tabs.
fn skip_space(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| *b != b' ' && *b != b'\t')
        .unwrap_or(bytes.len());
    bytes.get(start..).unwrap_or_default()
}

/// Whether `b` may be in an HTTP token.
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Up to [`MAX_ERROR_BODY`] bytes of a refused handshake's body, starting
/// with `raw`, read until `deadline`; whatever arrived is kept on a failure.
async fn read_error_body(
    io: &mut Box<dyn Io>,
    head: &Head,
    mut raw: Vec<u8>,
    deadline: Instant,
) -> Vec<u8> {
    if (100..200).contains(&head.status) || head.status == 204 || head.status == 304 {
        return Vec::new();
    }
    let chunked = head
        .headers
        .get_all(header::TRANSFER_ENCODING)
        .iter()
        .any(|value| {
            value
                .as_bytes()
                .split(|b| *b == b',')
                .any(|coding| coding.trim_ascii().eq_ignore_ascii_case(b"chunked"))
        });
    let length = if chunked {
        None
    } else {
        head.headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<usize>().ok())
    };
    let _ = timeout_at(deadline, async {
        loop {
            let (_, done) = error_body(&raw, chunked, length);
            if done || raw.len() >= MAX_ERROR_RAW {
                return;
            }
            raw.reserve(MAX_ERROR_BODY);
            match io.read_buf(&mut raw).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
    .await;
    error_body(&raw, chunked, length).0
}

/// The body in `raw` so far, and whether all that is wanted of it is there.
fn error_body(raw: &[u8], chunked: bool, length: Option<usize>) -> (Vec<u8>, bool) {
    if chunked {
        return dechunk(raw, MAX_ERROR_BODY);
    }
    let want = length.map_or(MAX_ERROR_BODY, |length| length.min(MAX_ERROR_BODY));
    let body = raw.get(..want.min(raw.len())).unwrap_or_default().to_vec();
    let done = raw.len() >= want;
    (body, done)
}

/// Up to `limit` bytes of the chunked body in `raw`, and whether it ended
/// or reached the limit (a malformed one ends where it breaks).
pub(super) fn dechunk(raw: &[u8], limit: usize) -> (Vec<u8>, bool) {
    let (mut body, done) = dechunk_all(raw, limit);
    body.truncate(limit);
    (body, done)
}

fn dechunk_all(raw: &[u8], limit: usize) -> (Vec<u8>, bool) {
    let mut body = Vec::new();
    let mut rest = raw;
    loop {
        if body.len() >= limit {
            return (body, true);
        }
        let Some(end) = rest.windows(2).position(|pair| pair == b"\r\n") else {
            return (body, false);
        };
        let line = rest.get(..end).unwrap_or_default();
        let size = line
            .split(|b| *b == b';')
            .next()
            .and_then(|size| std::str::from_utf8(size.trim_ascii()).ok())
            .and_then(|size| usize::from_str_radix(size, 16).ok());
        let Some(size) = size else {
            return (body, true);
        };
        if size == 0 {
            return (body, true);
        }
        rest = rest.get(end + 2..).unwrap_or_default();
        let take = size.min(rest.len());
        body.extend_from_slice(rest.get(..take).unwrap_or_default());
        if take < size {
            return (body, false);
        }
        rest = rest.get(size..).unwrap_or_default();
        match rest.strip_prefix(b"\r\n") {
            Some(next) => rest = next,
            None if rest.len() < 2 => return (body, false),
            None => return (body, true),
        }
    }
}

/// An error with `message`.
fn failed(message: impl Into<String>) -> ExecError {
    ExecError::new(ErrorKind::Upstream, message)
}

/// A network error that may clear on its own.
fn transient(message: impl Into<String>) -> ExecError {
    failed(message).with_transport(TransportFault::Transient)
}

/// The error for an I/O failure while doing `what`.
fn io_error(what: &str, error: &io::Error) -> ExecError {
    let message = format!("codex websockets executor: {what}: {error}");
    match error.kind() {
        io::ErrorKind::UnexpectedEof => failed(message).with_transport(TransportFault::Lifecycle),
        io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionRefused
        | io::ErrorKind::BrokenPipe
        | io::ErrorKind::TimedOut => transient(message),
        _ => failed(message),
    }
}
