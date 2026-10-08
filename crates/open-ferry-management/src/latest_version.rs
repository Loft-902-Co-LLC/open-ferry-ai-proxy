// Ported from CLIProxyAPI internal/api/handlers/management/config_basic.go
// (GetLatestVersion, setLatestReleaseRequestHeaders, releaseInfo),
// internal/util/github.go (ResolveGitHubToken) and
// internal/githubauth/token.go (ResolveToken) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `GET /v0/management/latest-version` (also
//! `/v8/management/server/latest-version`): the latest open-ferry release,
//! as GitHub names it.
//!
//! GitHub's API is asked for the latest release, through the config's
//! `proxy-url` when it names a proxy, with a 10 second limit on the whole
//! exchange. The answer is `{"latest-version":<tag>}`, or the release's
//! name when its tag is empty. The release is read as Go's
//! `json.Decoder.Decode` reads it: the answer is given as soon as its JSON
//! value is complete, and whatever follows is ignored. The config's
//! `server.github-token`, else a token from `GITHUB_TOKEN` or
//! `github_token`, is sent as a bearer token. Failures are
//! answered with a 502 naming what went wrong: `request_failed`,
//! `unexpected_status` (with the start of GitHub's answer), `decode_failed`
//! or `invalid_response`.
//!
//! Deviations from upstream:
//! - It asks for open-ferry's releases, as `open-ferry/<version>`, where
//!   upstream asks for CLIProxyAPI's, as `CLIProxyAPI`.
//! - Without a proxy in `proxy-url` the request goes direct; upstream's
//!   then follows the environment's `HTTP_PROXY` and `HTTPS_PROXY`.
//! - Redirects aren't followed: one is answered as `unexpected_status`.
//! - The token isn't taken from `GITSTORE_GIT_TOKEN`, and one that isn't
//!   UTF-8 is ignored.
//! - Errors are worded as Rust's HTTP client words them, without the URL;
//!   a release that doesn't decode is `decode_failed` with `EOF` when the
//!   body is empty and a fixed message otherwise, where upstream gives
//!   Go's decoder's error. At most 16 MiB of a release is read.
//! - A release that comes in so many pieces that checking after each
//!   would parse more than 16 MiB is checked after that only where its
//!   brackets close. So a body that isn't JSON may be read on until it
//!   ends, the limit or the deadline, where Go's decoder stops at the first
//!   byte that can't be JSON.

use std::time::Duration;

use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use http::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use open_ferry_translate::go::trim_space;
use serde::de::{IgnoredAny, MapAccess};
use tokio::time::{Instant, timeout_at};

use crate::Route;
use crate::api_call::{MAX_RESPONSE_BODY, error_chain};
use crate::bind::{self, GoStruct, set_string};
use crate::json::{self, Json};
use crate::proxy::{self, Setting};
use crate::state::ManagementState;

/// Where open-ferry's latest release is found. Upstream asks for
/// CLIProxyAPI's.
pub(crate) const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest";

/// The `User-Agent` the request is sent with.
const RELEASE_USER_AGENT: &str = concat!("open-ferry/", env!("CARGO_PKG_VERSION"));

/// How long the whole exchange may take (upstream's client timeout).
const TIMEOUT: Duration = Duration::from_secs(10);

/// How much of an unexpected answer is quoted.
const MAX_QUOTED: usize = 1024;

/// What a release says of itself (upstream's `releaseInfo`).
#[derive(Default)]
struct ReleaseInfo {
    tag_name: String,
    name: String,
}

impl GoStruct for ReleaseInfo {
    const FIELDS: &'static [&'static str] = &["tag_name", "name"];

    fn set<'de, A: MapAccess<'de>>(&mut self, index: usize, map: &mut A) -> Result<(), A::Error> {
        match index {
            0 => set_string(&mut self.tag_name, map),
            _ => set_string(&mut self.name, map),
        }
    }
}

/// The routes this module serves.
pub(crate) fn routes() -> Vec<Route> {
    vec![
        Route::key("/v0/management/latest-version", get(latest_version)),
        Route::key("/v8/management/server/latest-version", get(latest_version)),
    ]
}

/// `GET /v0/management/latest-version` (upstream's `GetLatestVersion`).
async fn latest_version(State(state): State<ManagementState>) -> Response {
    let deadline = Instant::now() + TIMEOUT;
    let url = match url::Url::parse(state.latest_release_url()) {
        Ok(url) => url,
        Err(error) => {
            return failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                "request_create_failed",
                error.to_string().into_bytes(),
            );
        }
    };
    let token = github_token(&state.config().github_token);
    let Some(headers) = release_headers(token.as_deref()) else {
        return request_failed("invalid header field value for \"Authorization\"");
    };
    let route = match proxy::parse(&state.config().proxy_url) {
        Some(Setting::Proxy(url)) => proxy::Route::Proxy(url),
        _ => proxy::Route::Direct,
    };
    let client = match state.clients().get(&route, false) {
        Ok(client) => client,
        Err(error) => return request_failed(&error_chain(&error.without_url())),
    };
    let mut request = reqwest::Request::new(Method::GET, url);
    *request.headers_mut() = headers;
    let mut response = match timeout_at(deadline, client.execute(request)).await {
        Err(_) => return request_failed("timed out awaiting headers"),
        Ok(Err(error)) => return request_failed(&error_chain(&error.without_url())),
        Ok(Ok(response)) => response,
    };

    let status = response.status();
    if status != StatusCode::OK {
        let body = read_body(&mut response, MAX_QUOTED, deadline).await;
        let mut message = format!("status {}: ", status.as_u16()).into_bytes();
        message.extend_from_slice(trim_space(&body));
        return failure(StatusCode::BAD_GATEWAY, "unexpected_status", message);
    }
    let body = read_release(&mut response, deadline).await;
    let Some(info) = bind::decode::<ReleaseInfo>(&body) else {
        let message = if body
            .iter()
            .all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        {
            "EOF"
        } else {
            "the release isn't a JSON object of strings"
        };
        return failure(
            StatusCode::BAD_GATEWAY,
            "decode_failed",
            message.as_bytes().to_vec(),
        );
    };
    let version = match info.tag_name.trim() {
        "" => info.name.trim(),
        tag => tag,
    };
    if version.is_empty() {
        return failure(
            StatusCode::BAD_GATEWAY,
            "invalid_response",
            b"missing release version".to_vec(),
        );
    }
    json::response(
        StatusCode::OK,
        &Json::map([("latest-version", Json::Str(version.to_owned()))]),
    )
}

/// The request's headers (upstream's `setLatestReleaseRequestHeaders`):
/// `None` when the token can't be sent in a header.
fn release_headers(token: Option<&str>) -> Option<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    headers.insert(USER_AGENT, HeaderValue::from_static(RELEASE_USER_AGENT));
    if let Some(token) = token.filter(|token| !token.is_empty()) {
        let mut value = HeaderValue::from_bytes(format!("Bearer {token}").as_bytes()).ok()?;
        value.set_sensitive(true);
        headers.insert(AUTHORIZATION, value);
    }
    Some(headers)
}

/// The GitHub token (upstream's `ResolveGitHubToken`): `configured`, the
/// config's `server.github-token`, trimmed, else one from the environment.
/// Tests never take one from the environment, so that none reaches a test's
/// server.
fn github_token(configured: &str) -> Option<String> {
    let configured = configured.trim();
    if !configured.is_empty() {
        return Some(configured.to_owned());
    }
    if cfg!(test) {
        return None;
    }
    token_from(|name| std::env::var(name).ok())
}

/// The first of `GITHUB_TOKEN` and `github_token` that `var` gives,
/// trimmed, that isn't empty.
fn token_from(var: impl Fn(&str) -> Option<String>) -> Option<String> {
    ["GITHUB_TOKEN", "github_token"]
        .into_iter()
        .find_map(|name| {
            let token = var(name)?;
            let token = token.trim();
            (!token.is_empty()).then(|| token.to_owned())
        })
}

/// Up to `limit` bytes of the body, or as much of it as arrives before
/// `deadline` or an error.
async fn read_body(response: &mut reqwest::Response, limit: usize, deadline: Instant) -> Vec<u8> {
    let mut body = Vec::new();
    while body.len() < limit {
        let Ok(Ok(Some(chunk))) = timeout_at(deadline, response.chunk()).await else {
            break;
        };
        let room = limit - body.len();
        body.extend(chunk.iter().take(room));
    }
    body
}

/// The body until its first JSON value is complete or can't be one, as
/// Go's `json.Decoder.Decode` reads it; else as much of it as arrives, up
/// to [`MAX_RESPONSE_BODY`] bytes, before `deadline`, an error or its end.
/// Anything read after the value is left for [`bind::decode`] to ignore.
async fn read_release(response: &mut reqwest::Response, deadline: Instant) -> Vec<u8> {
    let mut release = Release::default();
    while release.body.len() < MAX_RESPONSE_BODY {
        let Ok(Ok(Some(chunk))) = timeout_at(deadline, response.chunk()).await else {
            break;
        };
        if release.push(&chunk) {
            break;
        }
    }
    release.body
}

/// How much parsing a [`Release`] may spend checking whether its body is
/// complete after each chunk.
const CHECK_BUDGET: usize = MAX_RESPONSE_BODY;

/// A release's body as it arrives.
#[derive(Default)]
struct Release {
    body: Vec<u8>,
    /// Bytes parsed so far by checks after a chunk.
    checked: usize,
    /// Open objects and arrays outside strings.
    depth: usize,
    /// Whether the bytes so far end inside a string.
    in_string: bool,
    /// Whether they end after a `\` in a string.
    escaped: bool,
}

impl Release {
    /// Adds `chunk`, up to [`MAX_RESPONSE_BODY`] bytes in all; true once
    /// the first JSON value is complete or can't be one. Each check parses
    /// the body from its start, so once they have parsed
    /// [`CHECK_BUDGET`] bytes, the body is checked only where its
    /// brackets close, as a release's do once, at its end.
    fn push(&mut self, chunk: &[u8]) -> bool {
        let room = MAX_RESPONSE_BODY - self.body.len();
        let chunk = chunk.get(..room).unwrap_or(chunk);
        self.body.extend_from_slice(chunk);
        let closed = self.brackets_close(chunk);
        let affordable = self.checked + self.body.len() <= CHECK_BUDGET;
        if affordable {
            self.checked += self.body.len();
        }
        (closed || affordable) && first_value_read(&self.body)
    }

    /// Whether the brackets open outside strings all close in `chunk`.
    fn brackets_close(&mut self, chunk: &[u8]) -> bool {
        let mut closed = false;
        for &byte in chunk {
            if self.in_string {
                if self.escaped {
                    self.escaped = false;
                } else if byte == b'\\' {
                    self.escaped = true;
                } else if byte == b'"' {
                    self.in_string = false;
                }
                continue;
            }
            match byte {
                b'"' => self.in_string = true,
                b'{' | b'[' => self.depth += 1,
                b'}' | b']' => {
                    self.depth = self.depth.saturating_sub(1);
                    closed |= self.depth == 0;
                }
                _ => {}
            }
        }
        closed
    }
}

/// Whether Go's decoder is done with the first JSON value in `body`: it has
/// all of it, or has found that it isn't JSON. Go knows an object or an
/// array is over at its last byte, any other value only at the byte after
/// it, or at the end of the body. Bytes that aren't UTF-8 are read as
/// U+FFFD, as Go's decoder reads them in a string.
fn first_value_read(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body);
    let mut values = serde_json::Deserializer::from_str(&text).into_iter::<IgnoredAny>();
    match values.next() {
        None => false,
        Some(Ok(_)) => {
            let start = text.trim_start_matches([' ', '\t', '\n', '\r']);
            start.starts_with(['{', '[']) || values.byte_offset() < text.len()
        }
        Some(Err(error)) => !error.is_eof(),
    }
}

/// `{"error":"request_failed","message":message}` with a 502.
fn request_failed(message: &str) -> Response {
    failure(
        StatusCode::BAD_GATEWAY,
        "request_failed",
        message.as_bytes().to_vec(),
    )
}

/// `{"error":error,"message":message}` with `status`.
fn failure(status: StatusCode, error: &str, message: Vec<u8>) -> Response {
    json::response(
        status,
        &Json::map([
            ("error", Json::Str(error.to_owned())),
            ("message", Json::Bytes(message)),
        ]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ported from upstream's config_basic_version_test.go
    /// (TestSetLatestReleaseRequestHeaders): a token is sent as a bearer
    /// token, none without one, and the `Accept` and `User-Agent` always.
    #[test]
    fn release_requests_carry_the_token_when_there_is_one() {
        let cases = [
            (Some("release-token"), Some("Bearer release-token")),
            (None, None),
        ];
        for (token, want) in cases {
            let Some(headers) = release_headers(token) else {
                panic!("the headers for {token:?} are made");
            };
            let authorization = headers.get(AUTHORIZATION);
            assert_eq!(
                authorization.and_then(|value| value.to_str().ok()),
                want,
                "{token:?}"
            );
            assert!(authorization.is_none_or(HeaderValue::is_sensitive));
            assert_eq!(headers[ACCEPT], "application/vnd.github+json");
            assert_eq!(headers[USER_AGENT], RELEASE_USER_AGENT);
        }
        assert!(RELEASE_USER_AGENT.starts_with("open-ferry/"));
    }

    /// Not upstream's: reading stops where Go's decoder stops reading a
    /// value: at the end of an object or array, a byte after any other
    /// value, or a byte that can't go on.
    #[test]
    fn reading_stops_where_go_stops() {
        for (body, done) in [
            (&b""[..], false),
            (b" \n", false),
            (b"{\"tag_name\":\"v", false),
            (b"{\"tag_name\":\"v\xff1\"", false),
            (b"{\"tag_name\":\"v1\"}", true),
            (b" [1, {}] x", true),
            (b"[1,", false),
            (b"null", false),
            (b"null ", true),
            (b"\"v1\"", false),
            (b"\"v1\"\n", true),
            (b"12", false),
            (b"12x", true),
            (b"{bad", true),
            (b"\xff", true),
        ] {
            assert_eq!(
                first_value_read(body),
                done,
                "{:?}",
                body.escape_ascii().to_string()
            );
        }
    }

    /// Not upstream's: a release that arrives a byte at a time is checked
    /// after each byte only until the checks have parsed 16 MiB, then where
    /// its brackets close, so reading it takes time in proportion to its
    /// length. Brackets in strings don't count.
    #[test]
    fn checking_a_release_is_bounded() {
        let pad = "x".repeat(200_000);
        let text = format!(r#"{{"tag_name":"v1","body":"}}]{pad}\"{{","more":[{{}}]}} "#);
        let started = std::time::Instant::now();
        let mut release = Release::default();
        let bytes = text.as_bytes();
        let last = bytes.len() - 2;
        for (i, byte) in bytes.iter().enumerate() {
            assert_eq!(release.push(&[*byte]), i == last, "byte {i}");
            if i == last {
                break;
            }
        }
        assert!(release.checked <= CHECK_BUDGET);
        assert!(started.elapsed() < Duration::from_secs(20));

        // Within the budget, a byte that can't be JSON ends it at once.
        let mut release = Release::default();
        assert!(!release.push(b"{\"tag_name\":"));
        assert!(release.push(b"x"));
    }

    /// Not upstream's: the token is `GITHUB_TOKEN`, else `github_token`,
    /// trimmed; a blank one doesn't count; one that can't go in a header
    /// fails; and tests never take one from the environment.
    #[test]
    fn the_token_comes_from_the_environment() {
        let both = |name: &str| match name {
            "GITHUB_TOKEN" => Some(" upper \t".to_owned()),
            _ => Some("lower".to_owned()),
        };
        assert_eq!(token_from(both).as_deref(), Some("upper"));
        let blank = |name: &str| match name {
            "GITHUB_TOKEN" => Some("   ".to_owned()),
            _ => Some("lower".to_owned()),
        };
        assert_eq!(token_from(blank).as_deref(), Some("lower"));
        assert_eq!(token_from(|_| None), None);
        assert!(release_headers(Some("bad\rtoken")).is_none());
        assert!(github_token("").is_none());
        assert!(github_token(" \t").is_none());
    }

    /// Ported from upstream's githubauth/token_test.go (TestGlobalToken)
    /// and api/github_token_test.go: the config's token, trimmed, comes
    /// first; a blank one leaves the environment's.
    #[test]
    fn the_configured_token_comes_first() {
        assert_eq!(
            github_token(" config-token ").as_deref(),
            Some("config-token")
        );
        assert_eq!(github_token("replacement").as_deref(), Some("replacement"));
    }
}
