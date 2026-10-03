// Ported from CLIProxyAPI internal/access/config_access/provider.go,
// sdk/access/manager.go and AuthMiddleware in
// internal/api/server_middleware.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Client keys.
//!
//! A client presents a key as `Authorization: Bearer <key>`,
//! `X-Goog-Api-Key`, `X-Api-Key`, or the `key` or `auth_token` query
//! parameter. Unlike upstream, keys are compared in constant time, and the
//! client's key never reaches an executor.

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use http::{HeaderMap, HeaderName, header};
use open_ferry_translate::go;
use subtle::ConstantTimeEq;

use crate::errors::{JSON_UTF8, error_response};
use crate::query;
use crate::state::AppState;

/// The headers a client key can come in.
const KEY_HEADERS: [HeaderName; 3] = [
    header::AUTHORIZATION,
    HeaderName::from_static("x-goog-api-key"),
    HeaderName::from_static("x-api-key"),
];

/// The query parameters a client key can come in.
const KEY_PARAMS: [&str; 2] = ["key", "auth_token"];

/// Why a request was turned away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rejection {
    /// It presented no key.
    Missing,
    /// No key it presented is configured.
    Invalid,
}

impl Rejection {
    fn message(self) -> &'static str {
        match self {
            Self::Missing => "Missing API key",
            Self::Invalid => "Invalid API key",
        }
    }
}

/// Lets a request through if it presents a configured key, or if no keys
/// are configured. Otherwise answers 401.
pub(crate) async fn require_key(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let settings = state.settings();
    if !settings.keys.is_empty() {
        let params = query::parse(request.uri().query().unwrap_or(""));
        if let Err(rejection) = check(&settings.keys, request.headers(), &params) {
            return rejected(rejection);
        }
    }
    drop(settings);
    next.run(request).await
}

/// The 401 for a rejected request.
pub(crate) fn rejected(rejection: Rejection) -> Response {
    let body = serde_json::json!({ "error": rejection.message() }).to_string();
    error_response(401, HeaderMap::new(), Bytes::from(body), JSON_UTF8)
}

/// Checks the key a request presents against `keys`, which is not empty.
pub(crate) fn check(
    keys: &[String],
    headers: &HeaderMap,
    params: &[(String, String)],
) -> Result<(), Rejection> {
    let header_value = |name: &HeaderName| headers.get(name).map_or(&b""[..], |v| v.as_bytes());
    let authorization = header_value(&KEY_HEADERS[0]);
    let google = header_value(&KEY_HEADERS[1]);
    let anthropic = header_value(&KEY_HEADERS[2]);
    let query_key = query::first(params, KEY_PARAMS[0]).unwrap_or("").as_bytes();
    let query_token = query::first(params, KEY_PARAMS[1]).unwrap_or("").as_bytes();
    let candidates = [
        bearer_token(authorization),
        google,
        anthropic,
        query_key,
        query_token,
    ];
    if [authorization, google, anthropic, query_key, query_token]
        .iter()
        .all(|value| value.is_empty())
    {
        return Err(Rejection::Missing);
    }
    let matches = |candidate: &[u8]| {
        keys.iter().fold(false, |found, key| {
            found | bool::from(key.as_bytes().ct_eq(candidate))
        })
    };
    if candidates
        .iter()
        .any(|candidate| !candidate.is_empty() && matches(candidate))
    {
        Ok(())
    } else {
        Err(Rejection::Invalid)
    }
}

/// The token in an `Authorization` value (`extractBearerToken`): what
/// follows `Bearer ` (any case), trimmed, or the whole value when it has no
/// such prefix.
fn bearer_token(value: &[u8]) -> &[u8] {
    let Some(space) = value.iter().position(|&b| b == b' ') else {
        return value;
    };
    if !value[..space].eq_ignore_ascii_case(b"bearer") {
        return value;
    }
    go::trim_space(&value[space + 1..])
}

/// Removes the client's key from the headers and query parameters an
/// executor sees.
pub(crate) fn strip_credentials(headers: &mut HeaderMap, params: &mut Vec<(String, String)>) {
    for name in &KEY_HEADERS {
        headers.remove(name);
    }
    params.retain(|(name, _)| !KEY_PARAMS.contains(&name.as_str()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(*name, HeaderValue::from_static(value));
        }
        headers
    }

    fn params(query: &str) -> Vec<(String, String)> {
        query::parse(query)
    }

    #[test]
    fn checks_keys_as_upstream_does() {
        let keys = vec!["k1".to_owned(), "k2".to_owned()];
        let check =
            |h: &[(&'static str, &'static str)], q: &str| check(&keys, &headers(h), &params(q));
        assert_eq!(check(&[], ""), Err(Rejection::Missing));
        assert_eq!(check(&[("authorization", "Bearer k1")], ""), Ok(()));
        assert_eq!(check(&[("authorization", "bEaReR   k2 ")], ""), Ok(()));
        // A bare value is the key.
        assert_eq!(check(&[("authorization", "k1")], ""), Ok(()));
        assert_eq!(
            check(&[("authorization", "Basic k1")], ""),
            Err(Rejection::Invalid)
        );
        assert_eq!(check(&[("x-api-key", "k2")], ""), Ok(()));
        assert_eq!(check(&[("x-goog-api-key", "k1")], ""), Ok(()));
        assert_eq!(check(&[], "key=k1"), Ok(()));
        assert_eq!(check(&[], "auth_token=k2"), Ok(()));
        // Any candidate may match.
        assert_eq!(check(&[("authorization", "Bearer bad")], "key=k1"), Ok(()));
        // An empty bearer token is skipped, but still counts as a key given.
        assert_eq!(
            check(&[("authorization", "Bearer  ")], ""),
            Err(Rejection::Invalid)
        );
        // Only the first value counts.
        assert_eq!(
            check(&[("x-api-key", "bad"), ("x-api-key", "k1")], ""),
            Err(Rejection::Invalid)
        );
        assert_eq!(check(&[], "key=&auth_token="), Err(Rejection::Missing));
    }

    #[test]
    fn strips_credentials() {
        let mut h = headers(&[
            ("authorization", "Bearer k1"),
            ("x-api-key", "k1"),
            ("x-goog-api-key", "k1"),
            ("anthropic-version", "2023-06-01"),
        ]);
        let mut p = params("key=k1&auth_token=k1&alt=sse");
        strip_credentials(&mut h, &mut p);
        assert_eq!(h.len(), 1);
        assert_eq!(p, [("alt".to_owned(), "sse".to_owned())]);
    }
}
