//! Not upstream's: downloads: HTTPS only (plain HTTP to this machine),
//! redirects checked, sizes and times capped, the config's proxy used.

use std::time::Duration;

use url::Url;

use super::support::{ReleaseServer, Reply};
use crate::fetch::{self, Fetch, FetchError, HttpFetch};

const LIMIT: u64 = 1024;
const TIMEOUT: Duration = Duration::from_secs(10);

async fn get(server: &ReleaseServer, path: &str) -> Result<Vec<u8>, FetchError> {
    let url = server.base.join(path).unwrap();
    HttpFetch::new("").unwrap().get(&url, LIMIT, TIMEOUT).await
}

#[test]
fn a_base_url_must_be_https_or_this_machine() {
    let ok = [
        "https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases",
        "https://mirror.example/releases/",
        "http://127.0.0.1:8080/releases",
        "http://[::1]:8080/releases",
        "http://localhost/releases",
        "http://LOCALHOST:1/r",
    ];
    for text in ok {
        let url = fetch::parse_base_url(text).unwrap_or_else(|error| panic!("{text}: {error}"));
        assert!(!url.as_str().ends_with("releases/"), "{url}");
    }
    let refused = [
        "http://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases",
        "http://127.0.0.2/releases",
        "http://localhost.example/releases",
        "http://192.168.1.10/releases",
        "ftp://127.0.0.1/releases",
        "file:///tmp/releases",
        "https://user:secret@mirror.example/releases",
        "https://mirror.example/releases?x=1",
        "not a url",
    ];
    for text in refused {
        let error = fetch::parse_base_url(text).unwrap_err();
        assert!(matches!(error, FetchError::InsecureUrl(_)), "{text}");
        assert!(!error.to_string().contains("secret"), "{error}");
    }
}

#[tokio::test]
async fn a_plain_http_url_elsewhere_is_refused_before_any_connection() {
    let fetch = HttpFetch::new("").unwrap();
    // .invalid never resolves: refused before any lookup.
    let url = Url::parse("http://open-ferry.invalid/releases/latest/download/SHA256SUMS").unwrap();
    let error = fetch.get(&url, LIMIT, TIMEOUT).await.unwrap_err();
    assert!(matches!(error, FetchError::InsecureUrl(_)));
}

#[tokio::test]
async fn a_body_is_downloaded_with_the_user_agent() {
    let server = ReleaseServer::start().await;
    server.reply("/releases/a", Reply::Body(b"hello".to_vec()));
    assert_eq!(get(&server, "releases/a").await.unwrap(), b"hello");
    assert_eq!(server.user_agents(), [crate::USER_AGENT]);
    assert_eq!(crate::USER_AGENT, "open-ferry/0.0.0");
}

#[tokio::test]
async fn a_redirect_to_this_machine_is_followed() {
    let server = ReleaseServer::start().await;
    let target = format!("http://127.0.0.1:{}/objects/b", server.address.port());
    server.reply("/releases/a", Reply::Redirect(target));
    server.reply("/objects/b", Reply::Body(b"moved".to_vec()));
    assert_eq!(get(&server, "releases/a").await.unwrap(), b"moved");
    assert_eq!(server.paths(), ["/releases/a", "/objects/b"]);
}

#[tokio::test]
async fn a_redirect_to_plain_http_elsewhere_is_refused() {
    let server = ReleaseServer::start().await;
    server.reply(
        "/releases/a",
        Reply::Redirect("http://open-ferry.invalid/objects/b".into()),
    );
    let error = get(&server, "releases/a").await.unwrap_err();
    assert!(matches!(error, FetchError::Redirect(_)), "{error:?}");
    assert!(error.to_string().contains("open-ferry.invalid"));
    assert_eq!(server.paths(), ["/releases/a"]);
}

#[test]
fn a_download_over_https_is_never_redirected_to_plain_http() {
    let url = |text: &str| Url::parse(text).unwrap();
    let https = url("https://github.com/o/r/releases/latest/download/SHA256SUMS");
    let local = url("http://127.0.0.1:8080/releases/latest/download/SHA256SUMS");

    let next = url("http://127.0.0.1:9/b");
    let error = fetch::check_redirect(&next, std::slice::from_ref(&https)).unwrap_err();
    assert!(error.contains("plain http, after https"), "{error}");
    let after_both = [local, https];
    assert!(fetch::check_redirect(&url("http://localhost/b"), &after_both).is_err());

    // Over https, or plain http all the way on this machine, is fine.
    let [local, https] = after_both;
    let cdn = url("https://objects.example/b");
    assert_eq!(fetch::check_redirect(&cdn, &[https]), Ok(()));
    let local = [local];
    assert_eq!(fetch::check_redirect(&cdn, &local), Ok(()));
    let next = url("http://[::1]:8080/b");
    assert_eq!(fetch::check_redirect(&next, &local), Ok(()));
}

#[tokio::test]
async fn a_redirect_with_a_password_is_refused() {
    let server = ReleaseServer::start().await;
    let target = format!(
        "http://user:secret@127.0.0.1:{}/objects/b",
        server.address.port()
    );
    server.reply("/releases/a", Reply::Redirect(target));
    let error = get(&server, "releases/a").await.unwrap_err();
    assert!(matches!(error, FetchError::Redirect(_)), "{error:?}");
    assert!(!error.to_string().contains("secret"), "{error}");
    assert_eq!(server.requests(), 1);
}

#[tokio::test]
async fn endless_redirects_are_refused() {
    let server = ReleaseServer::start().await;
    let target = format!("http://127.0.0.1:{}/releases/a", server.address.port());
    server.reply("/releases/a", Reply::Redirect(target));
    let error = get(&server, "releases/a").await.unwrap_err();
    assert!(matches!(error, FetchError::Redirect(_)), "{error:?}");
    assert_eq!(server.requests(), fetch::MAX_REDIRECTS + 1);
}

#[tokio::test]
async fn an_answer_other_than_200_is_refused() {
    let server = ReleaseServer::start().await;
    server.reply("/releases/a", Reply::Status(500));
    assert_eq!(
        get(&server, "releases/a").await.unwrap_err(),
        FetchError::Status(500)
    );
    assert_eq!(
        get(&server, "releases/missing").await.unwrap_err(),
        FetchError::Status(404)
    );
}

#[tokio::test]
async fn a_body_over_the_limit_is_refused() {
    let server = ReleaseServer::start().await;
    server.reply("/releases/a", Reply::Body(vec![b'x'; 1025]));
    assert_eq!(
        get(&server, "releases/a").await.unwrap_err(),
        FetchError::TooLarge(LIMIT)
    );
    server.reply("/releases/b", Reply::Body(vec![b'x'; 1024]));
    assert_eq!(get(&server, "releases/b").await.unwrap().len(), 1024);
}

#[tokio::test]
async fn a_slow_answer_times_out() {
    let server = ReleaseServer::start().await;
    server.reply(
        "/releases/a",
        Reply::Slow(Duration::from_secs(30), b"late".to_vec()),
    );
    let url = server.base.join("releases/a").unwrap();
    let error = HttpFetch::new("")
        .unwrap()
        .get(&url, LIMIT, Duration::from_millis(200))
        .await
        .unwrap_err();
    assert_eq!(error, FetchError::TimedOut);
}

#[tokio::test]
async fn downloads_go_through_the_configs_proxy() {
    let proxy = ReleaseServer::start().await;
    proxy.reply("/releases/a", Reply::Body(b"via the proxy".to_vec()));
    // The URL's own port is bound but not listening: only the proxy answers.
    let closed = tokio::net::TcpSocket::new_v4().unwrap();
    closed.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let port = closed.local_addr().unwrap().port();
    let url = Url::parse(&format!("http://127.0.0.1:{port}/releases/a")).unwrap();
    let fetch = HttpFetch::new(&format!("http://{}", proxy.address)).unwrap();
    assert_eq!(
        fetch.get(&url, LIMIT, TIMEOUT).await.unwrap(),
        b"via the proxy"
    );
    assert_eq!(proxy.paths(), ["/releases/a"]);
    drop(closed);
}

#[test]
fn a_proxy_setting_is_read_without_repeating_it() {
    for none in ["", "  ", "direct", "DIRECT", "none"] {
        assert!(HttpFetch::new(none).is_ok(), "{none:?}");
    }
    assert!(HttpFetch::new("http://127.0.0.1:3128").is_ok());
    assert!(HttpFetch::new("https://user:secret@proxy.example:443").is_ok());
    for refused in [
        "socks5://user:secret@127.0.0.1:1080",
        "socks5h://127.0.0.1:1080",
    ] {
        let error = HttpFetch::new(refused).unwrap_err();
        assert!(error.to_string().contains("SOCKS"), "{error}");
        assert!(!error.to_string().contains("secret"), "{error}");
    }
    let error = HttpFetch::new("ftp://user:secret@proxy").unwrap_err();
    assert!(matches!(error, FetchError::Proxy(_)));
    assert!(!error.to_string().contains("secret"), "{error}");
}

#[test]
fn join_adds_a_path() {
    let base = fetch::parse_base_url("https://example.com/o/r/releases/").unwrap();
    assert_eq!(
        fetch::join(&base, "latest/download/SHA256SUMS")
            .unwrap()
            .as_str(),
        "https://example.com/o/r/releases/latest/download/SHA256SUMS"
    );
}
