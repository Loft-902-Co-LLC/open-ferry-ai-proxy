// Ported from CLIProxyAPI internal/util/header_helpers.go
// (ApplyCustomHeadersFromAttrs) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A credential's `header:<Name>` attributes: headers added to every request
//! made with it, over what the provider's executor set. A value of `$Name`
//! takes the client's `Name` header, and the header is left out when the
//! client sent none. Every provider applies them here.
//!
//! Deviations from upstream:
//! - A header that says which client is calling can't be set this way:
//!   `User-Agent`, `X-App`, any `X-Stainless-*`, `Originator`, `Session_id`,
//!   `Session-Id` and `X-Claude-Code-Session-Id`, in any case. Such an
//!   attribute is dropped with a warning that names the header but not the
//!   value, so these headers carry only what the client sent, or this
//!   project's own user agent. Upstream sets whatever is configured, which
//!   lets a credential pass for another client.
//! - A value that names `$CPA-SESSION-ID` is skipped, since session IDs
//!   aren't derived.
//! - An attribute whose name or value isn't a valid HTTP header is skipped
//!   with a warning; upstream's request would fail.

use std::collections::BTreeMap;

use http::{HeaderMap, HeaderName, HeaderValue};
use open_ferry_translate::go::to_upper;

use crate::json::eq_fold;

/// The client identity headers no attribute may set, lowercased as
/// [`HeaderName`] keeps them. Any `x-stainless-*` header counts too.
const IDENTITY_HEADERS: [&str; 6] = [
    "user-agent",
    "x-app",
    "originator",
    "session_id",
    "session-id",
    "x-claude-code-session-id",
];

/// Whether `name` says which client is calling, so no attribute may set it.
fn is_identity_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    IDENTITY_HEADERS.contains(&name) || name.starts_with("x-stainless-")
}

/// Applies the `header:<Name>` attributes in `attributes` to `target`, over
/// what is already set (`ApplyCustomHeadersFromAttrs`). `client` is the
/// client's request headers, for `$Name` values; `provider` starts the
/// warnings.
pub(crate) fn apply(
    target: &mut HeaderMap,
    attributes: &BTreeMap<String, String>,
    client: &HeaderMap,
    provider: &str,
) {
    for (key, value) in attributes {
        let Some(name) = key.strip_prefix("header:") else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim());
        if name.is_empty() || value.is_empty() {
            continue;
        }
        let Ok(header) = HeaderName::from_bytes(name.as_bytes()) else {
            tracing::warn!(
                "{provider}: custom header attribute {key:?} isn't a valid HTTP header; skipped"
            );
            continue;
        };
        if is_identity_header(&header) {
            tracing::warn!(
                "{provider}: custom header {name:?} would set the client's identity; skipped"
            );
            continue;
        }
        let value: &[u8] = match value.strip_prefix('$') {
            Some(variable) if eq_fold(variable.trim(), "CPA-SESSION-ID") => continue,
            _ if to_upper(value).contains("$CPA-SESSION-ID") => continue,
            Some(variable) => {
                let Some(client_value) = HeaderName::from_bytes(variable.trim().as_bytes())
                    .ok()
                    .and_then(|variable| client.get(variable))
                    .filter(|value| !value.is_empty())
                else {
                    continue;
                };
                client_value.as_bytes()
            }
            None => value.as_bytes(),
        };
        match HeaderValue::from_bytes(value) {
            Ok(value) => {
                target.insert(header, value);
            }
            Err(_) => tracing::warn!(
                "{provider}: custom header attribute {key:?} isn't a valid HTTP header; skipped"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attributes(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn identity_headers_in_any_case() {
        for name in [
            "User-Agent",
            "user-agent",
            "USER-AGENT",
            "uSeR-aGeNt",
            "X-App",
            "x-app",
            "X-APP",
            "X-Stainless-Lang",
            "x-stainless-runtime",
            "X-STAINLESS-PACKAGE-VERSION",
            "X-Stainless-Retry-Count",
            "Originator",
            "ORIGINATOR",
            "Session_id",
            "session_id",
            "SESSION_ID",
            "Session-Id",
            "session-id",
            "SESSION-ID",
            "X-Claude-Code-Session-Id",
            "x-claude-code-session-id",
            "X-CLAUDE-CODE-SESSION-ID",
        ] {
            let header = HeaderName::from_bytes(name.as_bytes()).unwrap();
            assert!(is_identity_header(&header), "{name}");
        }
        for name in [
            "X-Team",
            "X-Stainless",
            "Session",
            "X-Session-Id",
            "X-App-Version",
            "Anthropic-Beta",
            "Authorization",
        ] {
            let header = HeaderName::from_bytes(name.as_bytes()).unwrap();
            assert!(!is_identity_header(&header), "{name}");
        }
    }

    #[test]
    fn drops_identity_headers_and_keeps_the_rest() {
        let mut target = HeaderMap::new();
        target.insert("user-agent", HeaderValue::from_static("actual-client/1"));
        target.insert("originator", HeaderValue::from_static("actual-originator"));
        let mut client = HeaderMap::new();
        client.insert("x-source", HeaderValue::from_static("from-client"));
        client.insert("user-agent", HeaderValue::from_static("actual-client/1"));
        apply(
            &mut target,
            &attributes(&[
                ("header:User-Agent", "claude-cli/2.1.280"),
                ("header:x-app", "cli"),
                ("header:X-STAINLESS-RUNTIME", "node"),
                ("header:Originator", "codex-tui"),
                ("header:Session_id", "synthetic-session"),
                ("header:session-ID", "$X-Source"),
                ("header:X-Claude-Code-Session-Id", "s1"),
                ("header:X-Team", "blue"),
                ("header:X-Forward", "$X-Source"),
                ("header:X-Agent-Copy", "$User-Agent"),
            ]),
            &client,
            "test",
        );
        assert_eq!(target.get("user-agent").unwrap(), "actual-client/1");
        assert_eq!(target.get("originator").unwrap(), "actual-originator");
        for absent in [
            "x-app",
            "x-stainless-runtime",
            "session_id",
            "session-id",
            "x-claude-code-session-id",
        ] {
            assert!(target.get(absent).is_none(), "{absent}");
        }
        assert_eq!(target.get("x-team").unwrap(), "blue");
        assert_eq!(target.get("x-forward").unwrap(), "from-client");
        // Copying the client's own value into another header is fine.
        assert_eq!(target.get("x-agent-copy").unwrap(), "actual-client/1");
    }
}
