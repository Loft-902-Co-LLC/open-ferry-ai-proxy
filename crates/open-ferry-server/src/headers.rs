// Ported from CLIProxyAPI sdk/api/handlers/header_filter.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Which of a provider's response headers reach the client.
//!
//! Upstream also drops headers that AI gateways add (`x-litellm-`,
//! `helicone-` and so on) so that clients can't tell a gateway is in the way.
//! That is detection evasion, and isn't ported.

use http::{HeaderMap, HeaderName};

/// Hop-by-hop headers (RFC 7230 §6.1), `Set-Cookie`, and the headers the
/// server sets itself.
const HOP_BY_HOP: [&str; 11] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "set-cookie",
    "content-length",
    "content-encoding",
];

/// The response headers the server manages, which neither providers nor
/// errors may set.
const RESERVED: [&str; 7] = [
    "access-control-allow-credentials",
    "access-control-allow-headers",
    "access-control-allow-methods",
    "access-control-allow-origin",
    "access-control-expose-headers",
    "access-control-max-age",
    "x-cpa-trace-id",
];

/// Whether the server manages the response header `name`
/// (`IsCPAReservedResponseHeader`).
pub(crate) fn is_reserved_response_header(name: &HeaderName) -> bool {
    RESERVED.contains(&name.as_str())
}

/// A provider's response headers without the hop-by-hop, reserved, and
/// connection-scoped ones (`FilterUpstreamHeaders`).
pub(crate) fn filter_upstream_headers(src: &HeaderMap) -> HeaderMap {
    let mut scoped = Vec::new();
    for value in src.get_all(http::header::CONNECTION) {
        for token in String::from_utf8_lossy(value.as_bytes()).split(',') {
            let token = token.trim();
            if !token.is_empty() {
                scoped.push(token.to_ascii_lowercase());
            }
        }
    }
    let mut dst = HeaderMap::new();
    for (name, value) in src {
        let key = name.as_str();
        if HOP_BY_HOP.contains(&key)
            || RESERVED.contains(&key)
            || scoped.iter().any(|token| token == key)
        {
            continue;
        }
        dst.append(name.clone(), value.clone());
    }
    dst
}

/// Adds `src` to `dst`, skipping names `dst` already has a non-empty value
/// for (`WriteUpstreamHeaders`).
pub(crate) fn write_upstream_headers(dst: &mut HeaderMap, src: &HeaderMap) {
    for name in src.keys() {
        if dst.get(name).is_some_and(|value| !value.is_empty()) {
            continue;
        }
        for value in src.get_all(name) {
            dst.append(name.clone(), value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn filters_as_upstream_does() {
        let mut src = HeaderMap::new();
        for (name, value) in [
            ("connection", "keep-alive, X-Drop"),
            ("x-drop", "1"),
            ("set-cookie", "a=b"),
            ("content-length", "5"),
            ("access-control-allow-origin", "*"),
            ("x-request-id", "r1"),
            ("x-litellm-version", "kept"),
            ("openai-processing-ms", "12"),
        ] {
            src.append(name, HeaderValue::from_static(value));
        }
        src.append("openai-processing-ms", HeaderValue::from_static("13"));
        let filtered = filter_upstream_headers(&src);
        let mut names: Vec<&str> = filtered.keys().map(HeaderName::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["openai-processing-ms", "x-litellm-version", "x-request-id"]
        );
        assert_eq!(filtered.get_all("openai-processing-ms").iter().count(), 2);
    }

    #[test]
    fn writing_keeps_headers_already_set() {
        let mut dst = HeaderMap::new();
        dst.insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );
        dst.insert("x-empty", HeaderValue::from_static(""));
        let mut src = HeaderMap::new();
        src.insert("content-type", HeaderValue::from_static("application/json"));
        src.insert("x-empty", HeaderValue::from_static("filled"));
        src.insert("x-new", HeaderValue::from_static("1"));
        write_upstream_headers(&mut dst, &src);
        assert_eq!(dst["content-type"], "text/event-stream");
        assert_eq!(dst.get_all("x-empty").iter().count(), 2);
        assert_eq!(dst["x-new"], "1");
    }
}
