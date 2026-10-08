// Ported from the HttpRequest methods of CLIProxyAPI sdk/cliproxy/auth's
// ProviderExecutor and Manager (conductor.go, conductor_execution.go)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Plain HTTP calls made with a credential, outside the translated calls:
//! what [`ProviderExecutor::http_request`] sends and gets back, and the
//! Codex Alpha Search call a [`Dispatcher`](super::Dispatcher) makes.
//!
//! Deviations from upstream:
//! - A call is an [`HttpCall`] rather than an `http.Request`, and the
//!   executor reads the answer's body, up to the call's limit, where
//!   upstream hands the caller the open response.
//! - A call may name a path under the executor's own base URL
//!   ([`HttpTarget::Path`]), which tests point at a local server; upstream's
//!   caller always gives the whole URL.
//! - The `Debug` output of these types leaves out what may hold a secret:
//!   a URL's user info, query and fragment, header values and bodies.
//!
//! [`ProviderExecutor::http_request`]: crate::executor::ProviderExecutor::http_request

use std::fmt;
use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderMap, Method};

use crate::observe::Observation;

/// Where an [`HttpCall`] goes.
#[derive(Clone, PartialEq, Eq)]
pub enum HttpTarget {
    /// This URL.
    Url(String),
    /// This path under the executor's default base URL, such as
    /// `/alpha/search` under Codex's `https://chatgpt.com/backend-api/codex`.
    Path(String),
}

/// A request an executor sends with a credential, after adding the
/// credential's token and custom headers (upstream's `HttpRequest`, which
/// runs the executor's `PrepareRequest`).
#[derive(Clone)]
pub struct HttpCall {
    /// The method.
    pub method: Method,
    /// Where it goes.
    pub target: HttpTarget,
    /// The headers, which the executor adds the credential's own to.
    pub headers: HeaderMap,
    /// The body.
    pub body: Bytes,
    /// The client's headers, without its key, which a credential's custom
    /// header values may name (`$Name`).
    pub client_headers: HeaderMap,
    /// The most bytes of the answer's body to read; the rest is dropped.
    pub response_limit: usize,
    /// What the client request's observers see, for the request log
    /// (upstream's `RecordAPIRequest` and the calls after it).
    pub observation: Option<Arc<Observation>>,
}

impl fmt::Debug for HttpTarget {
    /// The target without its URL's user info, query or fragment.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Url(url) => f.debug_tuple("Url").field(&redact_url(url)).finish(),
            Self::Path(path) => f.debug_tuple("Path").field(&redact_url(path)).finish(),
        }
    }
}

impl fmt::Debug for HttpCall {
    /// The call without header values or the body.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpCall")
            .field("method", &self.method)
            .field("target", &self.target)
            .field("headers", &HeaderNames(&self.headers))
            .field("body_len", &self.body.len())
            .field("client_headers", &HeaderNames(&self.client_headers))
            .field("response_limit", &self.response_limit)
            .field("observation", &self.observation)
            .finish()
    }
}

/// The answer to an [`HttpCall`], whatever its status.
#[derive(Clone, Default)]
pub struct HttpReply {
    /// The HTTP status.
    pub status: u16,
    /// The response headers.
    pub headers: HeaderMap,
    /// The body, up to the call's limit.
    pub body: Bytes,
    /// Why the body ended early, when reading it failed; [`Self::body`] holds
    /// what was read.
    pub read_error: Option<String>,
}

impl fmt::Debug for HttpReply {
    /// The answer without header values or the body.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpReply")
            .field("status", &self.status)
            .field("headers", &HeaderNames(&self.headers))
            .field("body_len", &self.body.len())
            .field("read_error", &self.read_error)
            .finish()
    }
}

/// A Codex Alpha Search call (`POST /v1/alpha/search`), as the client made
/// it.
#[derive(Clone, Default)]
pub struct AlphaSearch {
    /// The client's payload.
    pub body: Bytes,
    /// The client's headers, without its key.
    pub headers: HeaderMap,
    /// What the client request's observers see.
    pub observation: Option<Arc<Observation>>,
}

impl fmt::Debug for AlphaSearch {
    /// The call without header values or the body.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AlphaSearch")
            .field("body_len", &self.body.len())
            .field("headers", &HeaderNames(&self.headers))
            .field("observation", &self.observation)
            .finish()
    }
}

/// A header map's names, for `Debug` output without the values.
pub(super) struct HeaderNames<'h>(pub(super) &'h HeaderMap);

impl fmt::Debug for HeaderNames<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.0.keys()).finish()
    }
}

/// `url` with its user info, query and fragment, which may hold secrets,
/// replaced by `[redacted]`.
pub(super) fn redact_url(url: &str) -> String {
    let (before, tail) = match url.find(['?', '#']) {
        Some(cut) => (
            url.get(..cut).unwrap_or_default(),
            url.get(cut..).unwrap_or_default(),
        ),
        None => (url, ""),
    };
    let mut out = String::with_capacity(url.len());
    match before.split_once("://") {
        Some((scheme, rest)) => {
            out.push_str(scheme);
            out.push_str("://");
            let (authority, path) = match rest.find(['/', '\\']) {
                Some(cut) => (
                    rest.get(..cut).unwrap_or_default(),
                    rest.get(cut..).unwrap_or_default(),
                ),
                None => (rest, ""),
            };
            match authority.rsplit_once('@') {
                Some((_, host)) => {
                    out.push_str("[redacted]@");
                    out.push_str(host);
                }
                None => out.push_str(authority),
            }
            out.push_str(path);
        }
        None => out.push_str(before),
    }
    if let Some(delimiter) = tail.chars().next() {
        out.push(delimiter);
        out.push_str("[redacted]");
    }
    out
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;

    use super::*;

    // Not upstream's: Debug output leaves out what may be a secret.
    #[test]
    fn debug_output_leaves_out_secrets() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer HEADER-SECRET"),
        );
        let mut client_headers = HeaderMap::new();
        client_headers.insert("x-tenant", HeaderValue::from_static("CLIENT-SECRET"));
        let call = HttpCall {
            method: Method::POST,
            target: HttpTarget::Url(
                "https://user:USERINFO-SECRET@codex.example.com:8443/v1/alpha/search?api_key=QUERY-SECRET#FRAGMENT-SECRET"
                    .into(),
            ),
            headers: headers.clone(),
            body: Bytes::from_static(br#"{"q":"BODY-SECRET"}"#),
            client_headers: client_headers.clone(),
            response_limit: 32,
            observation: None,
        };
        let reply = HttpReply {
            status: 200,
            headers,
            body: Bytes::from_static(b"BODY-SECRET"),
            read_error: None,
        };
        let search = AlphaSearch {
            body: Bytes::from_static(b"BODY-SECRET"),
            headers: client_headers,
            observation: None,
        };
        let path = HttpTarget::Path("/alpha/search?token=QUERY-SECRET".into());
        let text = format!("{call:?} {reply:?} {search:?} {path:?}");
        for secret in [
            "USERINFO-SECRET",
            "QUERY-SECRET",
            "FRAGMENT-SECRET",
            "HEADER-SECRET",
            "CLIENT-SECRET",
            "BODY-SECRET",
            "user:",
        ] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        assert_eq!(
            format!("{:?}", call.target),
            r#"Url("https://[redacted]@codex.example.com:8443/v1/alpha/search?[redacted]")"#
        );
        assert_eq!(format!("{path:?}"), r#"Path("/alpha/search?[redacted]")"#);
        assert!(text.contains(r#"headers: ["authorization"]"#), "{text}");
        assert!(text.contains("body_len: 19"), "{text}");
        assert!(text.contains("body_len: 11"), "{text}");
        assert!(text.contains(r#"client_headers: ["x-tenant"]"#), "{text}");
    }

    // Not upstream's: what a URL keeps.
    #[test]
    fn redacts_only_what_may_be_secret() {
        for (url, want) in [
            (
                "https://chatgpt.com/backend-api/codex/alpha/search",
                "https://chatgpt.com/backend-api/codex/alpha/search",
            ),
            ("http://a@b@host/p@q", "http://[redacted]@host/p@q"),
            ("http://host#frag", "http://host#[redacted]"),
            ("http://u@host\\p", "http://[redacted]@host\\p"),
            ("http://host?a=b://c@d", "http://host?[redacted]"),
            ("/alpha/search", "/alpha/search"),
            ("", ""),
        ] {
            assert_eq!(redact_url(url), want, "{url}");
        }
    }
}
