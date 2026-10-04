// Ported from CLIProxyAPI internal/api/handlers/management/config_basic.go
// (GetLatestVersion, setLatestReleaseRequestHeaders, releaseInfo) and
// internal/util/github.go (ResolveGitHubToken) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `GET /v0/management/latest-version` (also
//! `/v8/management/server/latest-version`): the latest open-ferry release,
//! as GitHub names it.
//!
//! GitHub's API is asked for the latest release, through the config's
//! `proxy-url` when it names a proxy, with a 10 second limit on the whole
//! exchange. The answer is `{"latest-version":<tag>}`, or the release's
//! name when its tag is empty. A token from `GITHUB_TOKEN` or
//! `github_token` is sent as a bearer token. Failures are answered with a
//! 502 naming what went wrong: `request_failed`, `unexpected_status`
//! (with the start of GitHub's answer), `decode_failed` or
//! `invalid_response`.
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

use std::time::Duration;

use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use http::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use open_ferry_translate::go::trim_space;
use serde::de::MapAccess;
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
    let Some(headers) = release_headers(github_token().as_deref()) else {
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
    let body = read_body(&mut response, MAX_RESPONSE_BODY, deadline).await;
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

/// The GitHub token from the environment (upstream's
/// `ResolveGitHubToken`). Tests never take one, so that none reaches a
/// test's server.
fn github_token() -> Option<String> {
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
        assert!(github_token().is_none());
    }
}
