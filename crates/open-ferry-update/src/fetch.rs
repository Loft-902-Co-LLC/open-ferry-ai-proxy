//! Downloading a release's files.
//!
//! Every URL, the base and each redirect, must be `https`; plain `http` is
//! allowed only to `127.0.0.1`, `::1` and `localhost`, for testing a
//! release server on the same machine. A URL with a user name or password
//! is refused. At most [`MAX_REDIRECTS`] redirects are followed.
//!
//! [`HttpFetch`] goes through the config's `proxy-url` (an `http` or
//! `https` proxy; `direct`, `none` or empty for none) and never through a
//! proxy from the environment, as the rest of open-ferry. A SOCKS proxy
//! is refused: open-ferry's HTTP client has none. Each answer must be a
//! 200 of at most the given size, read within the given time; nothing is
//! decompressed on the way.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use url::{Host, Url};

use crate::USER_AGENT;

/// A future that can be sent between threads.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The most redirects followed for one download.
pub const MAX_REDIRECTS: usize = 10;

/// How long connecting may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Downloads a URL's body.
pub trait Fetch: Send + Sync {
    /// The body at `url`: a 200 of at most `limit` bytes, all of it within
    /// `timeout`.
    fn get<'a>(
        &'a self,
        url: &'a Url,
        limit: u64,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>>;
}

/// Why a download failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchError {
    /// The URL isn't `https`, nor `http` to this machine, or has a user
    /// name or password.
    InsecureUrl(String),
    /// The proxy setting can't be used.
    Proxy(String),
    /// The answer's status wasn't 200.
    Status(u16),
    /// The body is over the limit, in bytes.
    TooLarge(u64),
    /// It took longer than allowed.
    TimedOut,
    /// A redirect was refused.
    Redirect(String),
    /// Connecting or reading failed.
    Network(String),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InsecureUrl(url) => write!(
                f,
                "{url} isn't an https URL (plain http is allowed only to 127.0.0.1, ::1 and localhost, and no URL may carry a user name or password)"
            ),
            Self::Proxy(message) => f.write_str(message),
            Self::Status(status) => write!(f, "the server answered {status}"),
            Self::TooLarge(limit) => write!(f, "the download is over {limit} bytes"),
            Self::TimedOut => f.write_str("the download took too long"),
            Self::Redirect(message) => write!(f, "a redirect was refused: {message}"),
            Self::Network(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for FetchError {}

/// Whether `url` names this machine as `127.0.0.1`, `::1` or `localhost`.
pub fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address == std::net::Ipv4Addr::LOCALHOST,
        Some(Host::Ipv6(address)) => address == std::net::Ipv6Addr::LOCALHOST,
        None => false,
    }
}

/// Whether updates may download from `url`: `https`, or `http` to this
/// machine, with no user name or password.
pub fn check_url(url: &Url) -> Result<(), FetchError> {
    let scheme_ok = match url.scheme() {
        "https" => url.host().is_some(),
        "http" => is_loopback(url),
        _ => false,
    };
    if scheme_ok && url.username().is_empty() && url.password().is_none() {
        Ok(())
    } else {
        Err(FetchError::InsecureUrl(shown(url)))
    }
}

/// `url` without a user name or password, for messages.
fn shown(url: &Url) -> String {
    let mut url = url.clone();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.to_string()
}

/// Reads a release base URL, such as
/// `https://github.com/<owner>/<repo>/releases`: trimmed, without a
/// trailing `/`, and allowed by [`check_url`].
pub fn parse_base_url(text: &str) -> Result<Url, FetchError> {
    let trimmed = text.trim().trim_end_matches('/');
    let url = Url::parse(trimmed).map_err(|_| FetchError::InsecureUrl(trimmed.to_owned()))?;
    check_url(&url)?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(FetchError::InsecureUrl(shown(&url)));
    }
    Ok(url)
}

/// `base` with `path` added after a `/`.
pub fn join(base: &Url, path: &str) -> Result<Url, FetchError> {
    let text = format!("{}/{path}", base.as_str().trim_end_matches('/'));
    Url::parse(&text).map_err(|_| FetchError::InsecureUrl(text))
}

/// The proxy to use for `proxy_url`, the config's setting: `None` for none.
fn proxy_for(proxy_url: &str) -> Result<Option<reqwest::Proxy>, FetchError> {
    let trimmed = proxy_url.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("direct")
        || trimmed.eq_ignore_ascii_case("none")
    {
        return Ok(None);
    }
    // The setting may carry a password, so no message repeats it.
    let invalid =
        || FetchError::Proxy("proxy-url isn't a proxy URL the update check can use".into());
    let url = Url::parse(trimmed).map_err(|_| invalid())?;
    match url.scheme() {
        "http" | "https" => reqwest::Proxy::all(url).map(Some).map_err(|_| invalid()),
        "socks5" | "socks5h" => Err(FetchError::Proxy(
            "the update check can't go through a SOCKS proxy (proxy-url); use an http or https proxy, or direct".into(),
        )),
        _ => Err(invalid()),
    }
}

/// A redirect [`HttpFetch`] refused.
#[derive(Debug)]
struct RefusedRedirect(String);

impl fmt::Display for RefusedRedirect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RefusedRedirect {}

/// Downloads over HTTP with `reqwest`.
#[derive(Clone, Debug)]
pub struct HttpFetch {
    client: reqwest::Client,
}

impl HttpFetch {
    /// A client through `proxy_url`, the config's setting.
    pub fn new(proxy_url: &str) -> Result<Self, FetchError> {
        let policy = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() > MAX_REDIRECTS {
                let error = RefusedRedirect(format!("more than {MAX_REDIRECTS} redirects"));
                return attempt.error(error);
            }
            match check_url(attempt.url()) {
                Ok(()) => attempt.follow(),
                Err(error) => {
                    let error = RefusedRedirect(error.to_string());
                    attempt.error(error)
                }
            }
        });
        let builder = reqwest::Client::builder()
            .redirect(policy)
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT);
        let builder = match proxy_for(proxy_url)? {
            Some(proxy) => builder.proxy(proxy),
            None => builder.no_proxy(),
        };
        let client = builder
            .build()
            .map_err(|error| FetchError::Network(format!("the HTTP client: {error}")))?;
        Ok(Self { client })
    }

    async fn download(&self, url: &Url, limit: u64) -> Result<Vec<u8>, FetchError> {
        let mut response = self.client.get(url.clone()).send().await.map_err(network)?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(FetchError::Status(response.status().as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > limit)
        {
            return Err(FetchError::TooLarge(limit));
        }
        let capacity = response.content_length().unwrap_or(0).min(limit);
        let mut body = Vec::with_capacity(usize::try_from(capacity).unwrap_or(0));
        while let Some(chunk) = response.chunk().await.map_err(network)? {
            let total = u64::try_from(body.len().saturating_add(chunk.len())).unwrap_or(u64::MAX);
            if total > limit {
                return Err(FetchError::TooLarge(limit));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

impl Fetch for HttpFetch {
    fn get<'a>(
        &'a self,
        url: &'a Url,
        limit: u64,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>> {
        Box::pin(async move {
            check_url(url)?;
            match tokio::time::timeout(timeout, self.download(url, limit)).await {
                Ok(result) => result,
                Err(_) => Err(FetchError::TimedOut),
            }
        })
    }
}

/// A `reqwest` error as a [`FetchError`].
fn network(error: reqwest::Error) -> FetchError {
    let mut source = std::error::Error::source(&error);
    while let Some(inner) = source {
        if let Some(refused) = inner.downcast_ref::<RefusedRedirect>() {
            return FetchError::Redirect(refused.0.clone());
        }
        source = inner.source();
    }
    if error.is_timeout() {
        return FetchError::TimedOut;
    }
    let error = error.without_url();
    let mut message = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(inner) = source {
        message.push_str(": ");
        message.push_str(&inner.to_string());
        source = inner.source();
    }
    FetchError::Network(message)
}
