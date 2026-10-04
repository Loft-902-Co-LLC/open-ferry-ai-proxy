//! Connecting: which proxy, the `CONNECT` tunnel, and a refused
//! handshake's body.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::ExecError;
use open_ferry_core::executor::ProviderExecutor;
use tokio::net::TcpListener;

use super::super::dial::{self, Route};
use super::super::mock::{Proxy, Server};
use super::{COMPLETED, HELLO, auth, collect, executor, refused, request, within, ws_options};
use crate::codex::CodexExecutor;
use crate::codex::client::USER_AGENT;

/// The proxy a route goes through, as `host:port`; `None` for none.
fn proxied(route: Result<Route, ExecError>) -> Option<String> {
    match route {
        Ok(Route::Direct) => None,
        Ok(Route::Connect(proxy)) => Some(format!("{}:{}", proxy.host, proxy.port)),
        Err(error) => panic!("no route: {error:?}"),
    }
}

/// An environment of `vars`.
fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |name| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_owned())
    }
}

// TestNewProxyAwareWebsocketDialerDirectDisablesProxy: the credential's
// `direct` wins over the global proxy.
#[test]
fn direct_disables_the_global_proxy() {
    let executor = CodexExecutor::new("http://global-proxy.example.com:8080");
    let direct = Auth {
        proxy_url: "direct".into(),
        ..Auth::default()
    };
    let no_env = |_: &str| None;
    let route = dial::route(
        &executor.proxy_for(&direct),
        true,
        "chatgpt.com",
        443,
        &no_env,
    );
    assert_eq!(proxied(route), None);

    let route = dial::route(
        &executor.proxy_for(&Auth::default()),
        true,
        "chatgpt.com",
        443,
        &no_env,
    );
    assert_eq!(
        proxied(route).as_deref(),
        Some("global-proxy.example.com:8080")
    );
}

// Not upstream's: a set proxy decides the route; an empty, unusable or
// unknown one falls back to the environment's.
#[test]
fn set_proxy_decides_the_route() {
    let vars = [("HTTPS_PROXY", "http://env-proxy.example:3128")];
    let env = env(&vars);
    let route = |proxy: &str| dial::route(proxy, true, "codex.example", 443, &env);
    assert_eq!(proxied(route("direct")), None);
    assert_eq!(proxied(route(" NONE ")), None);
    assert_eq!(
        proxied(route("http://explicit.example:8080")).as_deref(),
        Some("explicit.example:8080")
    );
    assert_eq!(
        proxied(route("http://explicit.example")).as_deref(),
        Some("explicit.example:80")
    );
    for fallback in ["", "ftp://ftp.example:21", "::not a url"] {
        assert_eq!(
            proxied(route(fallback)).as_deref(),
            Some("env-proxy.example:3128"),
            "{fallback:?}"
        );
    }
    let error = route("socks5h://user:secret@socks.example:1080").unwrap_err();
    assert_eq!(error.status, 502);
    assert!(error.message.contains("SOCKS5"), "{error:?}");
    assert!(!error.message.contains("secret"), "{error:?}");
    assert!(route("https://tls-proxy.example:443").is_err());
}

// Not upstream's: the environment's proxy, as Go's `ProxyFromEnvironment`
// reads it.
#[test]
fn environment_proxy_is_read_as_go_reads_it() {
    let route = |vars: &[(&str, &str)], tls: bool, host: &str| {
        proxied(dial::route(
            "",
            tls,
            host,
            if tls { 443 } else { 80 },
            &env(vars),
        ))
    };
    let http = [("HTTP_PROXY", "http://http-proxy.example:3128")];
    let https = [("https_proxy", "tls-proxy.example:3129")];
    assert_eq!(route(&[], true, "codex.example"), None);
    assert_eq!(
        route(&http, false, "codex.example").as_deref(),
        Some("http-proxy.example:3128")
    );
    assert_eq!(route(&http, true, "codex.example"), None);
    assert_eq!(
        route(&https, true, "codex.example").as_deref(),
        Some("tls-proxy.example:3129")
    );
    assert_eq!(route(&http, false, "127.0.0.1"), None);
    assert_eq!(route(&http, false, "localhost"), None);
    let excluded = [
        ("HTTP_PROXY", "http://http-proxy.example:3128"),
        ("NO_PROXY", "codex.example"),
    ];
    assert_eq!(route(&excluded, false, "codex.example"), None);
    assert_eq!(route(&excluded, false, "api.codex.example"), None);
    assert_eq!(
        route(&excluded, false, "other.example").as_deref(),
        Some("http-proxy.example:3128")
    );

    let cgi_http = [
        ("HTTP_PROXY", "http://http-proxy.example:3128"),
        ("REQUEST_METHOD", "GET"),
    ];
    assert!(dial::route("", false, "codex.example", 80, &env(&cgi_http)).is_err());
    let cgi_https = [
        ("HTTPS_PROXY", "http://tls-proxy.example:3129"),
        ("REQUEST_METHOD", "GET"),
    ];
    assert_eq!(
        route(&cgi_https, true, "codex.example").as_deref(),
        Some("tls-proxy.example:3129")
    );

    let socks = [("HTTPS_PROXY", "socks5://socks.example:1080")];
    let error = dial::route("", true, "codex.example", 443, &env(&socks)).unwrap_err();
    assert_eq!(error.status, 502);
}

// Not upstream's: Go's `useProxy` against `NO_PROXY`.
#[test]
fn no_proxy_matches_as_go_does() {
    let uses = |host: &str, port: u16, no_proxy: &str| dial::use_proxy(host, port, no_proxy);
    assert!(!uses("localhost", 80, ""));
    assert!(!uses("127.0.0.1", 80, ""));
    assert!(!uses("::1", 80, ""));
    assert!(uses("codex.example", 443, ""));
    assert!(!uses("codex.example", 443, "*"));

    assert!(!uses("api.example.com", 443, ".example.com"));
    assert!(uses("example.com", 443, ".example.com"));
    assert!(!uses("api.example.com", 443, "*.example.com"));
    assert!(uses("example.com", 443, "*.example.com"));
    assert!(!uses("example.com", 443, "example.com"));
    assert!(!uses("api.example.com", 443, "example.com"));
    assert!(uses("notexample.com", 443, "example.com"));

    assert!(!uses("10.1.2.3", 443, "10.0.0.0/8"));
    assert!(uses("11.0.0.1", 443, "10.0.0.0/8"));
    assert!(!uses("192.168.1.1", 443, "192.168.1.1"));
    assert!(uses("192.168.1.2", 443, "192.168.1.1"));

    assert!(!uses("codex.example", 443, "codex.example:443"));
    assert!(uses("codex.example", 80, "codex.example:443"));
    assert!(!uses("bar.example", 443, " foo.example , BAR.example "));
}

// Not upstream's: reading a refused handshake's chunked body.
#[test]
fn dechunk_reads_a_chunked_body() {
    assert_eq!(
        dial::dechunk(b"5\r\nhello\r\n0\r\n\r\n", 1024),
        (b"hello".to_vec(), true)
    );
    assert_eq!(
        dial::dechunk(b"3\r\nabc\r\n2;ext=1\r\nde\r\n0\r\n\r\n", 1024),
        (b"abcde".to_vec(), true)
    );
    assert_eq!(dial::dechunk(b"5\r\nhel", 1024), (b"hel".to_vec(), false));
    assert_eq!(
        dial::dechunk(b"5\r\nhello", 1024),
        (b"hello".to_vec(), false)
    );
    assert_eq!(dial::dechunk(b"5\r\nhello\r\n", 3), (b"hel".to_vec(), true));
    assert_eq!(dial::dechunk(b"zz\r\nhello", 1024), (Vec::new(), true));
}

// Not upstream's: a credential's SOCKS5 proxy is refused, as `api-call`
// refuses it, before anything is sent.
#[tokio::test]
async fn socks5_proxy_is_refused() {
    let server = Server::turns(&[COMPLETED]).await;
    let auth = Auth {
        proxy_url: "socks5://user:secret@127.0.0.1:1080".into(),
        ..auth(&server.url)
    };
    let error = refused(
        within(
            "the call",
            executor().execute_stream(
                Arc::new(auth),
                request("gpt-5-codex", HELLO),
                ws_options(""),
            ),
        )
        .await,
    );
    assert_eq!(error.status, 502);
    assert!(error.message.contains("SOCKS5"), "{error:?}");
    assert!(!error.message.contains("secret"), "{error:?}");
    assert!(server.record().handshakes.is_empty());
}

// Not upstream's: an HTTP proxy is asked to `CONNECT`, with the proxy URL's
// credentials and open-ferry's own `User-Agent`, and the call goes through
// the tunnel.
#[tokio::test]
async fn http_proxy_tunnels_with_connect() {
    let server = Server::turns(&[COMPLETED]).await;
    let proxy = Proxy::start().await;
    let authority = server.url.trim_start_matches("http://").to_owned();
    let auth = Auth {
        proxy_url: proxy.url.replace("http://", "http://user:p%40ss@"),
        ..auth(&server.url)
    };
    let response = executor()
        .execute_stream(
            Arc::new(auth),
            request("gpt-5-codex", HELLO),
            ws_options(""),
        )
        .await
        .unwrap();
    let (chunks, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(chunks.len(), 1);

    let connects = proxy.connects();
    assert_eq!(connects.len(), 1);
    let connect = &connects[0];
    assert_eq!(connect.method, "CONNECT");
    assert_eq!(connect.path, authority);
    assert_eq!(connect.header("host"), Some(authority.as_str()));
    let basic = format!("Basic {}", STANDARD.encode("user:p@ss"));
    assert_eq!(connect.header("proxy-authorization"), Some(basic.as_str()));
    assert_eq!(connect.header("user-agent"), Some(USER_AGENT));
    assert!(connect.header("authorization").is_none());

    let record = server.record();
    assert_eq!(record.handshakes.len(), 1);
    assert_eq!(
        record.handshakes[0].header("authorization"),
        Some("Bearer sk-test")
    );
}

// Not upstream's: a `CONNECT` the proxy refuses fails the call with the
// proxy's status.
#[tokio::test]
async fn refused_connect_fails_the_call() {
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let proxy = Proxy::start().await;
    let auth = Auth {
        proxy_url: proxy.url.clone(),
        ..auth(&base_url)
    };
    let error = refused(
        within(
            "the call",
            executor().execute_stream(
                Arc::new(auth),
                request("gpt-5-codex", HELLO),
                ws_options(""),
            ),
        )
        .await,
    );
    assert!(
        error.message.contains("proxy CONNECT failed: 502"),
        "{error:?}"
    );
    assert_eq!(proxy.connects().len(), 1);
}
