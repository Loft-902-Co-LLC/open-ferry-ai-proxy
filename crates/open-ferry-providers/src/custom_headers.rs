// Ported from CLIProxyAPI internal/util/header_helpers.go
// (ApplyCustomHeadersFromAttrs) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A credential's `header:<Name>` attributes: headers added to every request
//! made with it, over what the provider's executor set. A value of `$Name`
//! takes the client's `Name` header, and the header is left out when the
//! client sent none. Every provider applies them here.
//!
//! `Content-Length`, `Transfer-Encoding` and `Trailer` attributes are
//! skipped, as Go's HTTP client ignores those headers and frames the body
//! itself. A `Host` attribute sets the request's host, as upstream's does.
//!
//! Deviations from upstream:
//! - A header that says which client is calling, or which of its
//!   sessions, conversations, threads, windows, agents or containers, can't
//!   be set this way: `User-Agent`, `X-App`, any `X-Stainless-*`,
//!   `Originator`, `Session_id`, `Session-Id`, `Conversation_id`,
//!   `Conversation-Id`, `Thread-Id`, `Thread_id`, `X-Codex-Window-Id`,
//!   `X-Claude-Code-Session-Id`, `X-Claude-Code-Agent-Id`,
//!   `X-Claude-Code-Parent-Agent-Id`, `X-Claude-Remote-Session-Id`,
//!   `X-Claude-Remote-Container-Id`, and the vendors' own: `X-Goog-Api-Client`
//!   (Google's API client), `X-Client-Id` (Meta's client), `X-Xai-Token-Auth`
//!   (xAI's client token) and any `X-Msh-*` (Kimi's platform, version and
//!   device) or `X-Grok-Client-*` (xAI's client version and identifier), in
//!   any case. Such an attribute is dropped with a warning that
//!   names the header but not the value, whether its value is a literal or
//!   a `$Name` taken from the client, so these headers carry only what the
//!   client sent, or this project's own user agent. Upstream sets whatever
//!   is configured, which lets a credential pass for another client or one
//!   of its sessions.
//! - A value that names `$CPA-SESSION-ID` is skipped, since session IDs
//!   aren't derived.
//! - An attribute whose name or value isn't a valid HTTP header is skipped
//!   with a warning; upstream's request would fail.

use std::collections::BTreeMap;

use http::{HeaderMap, HeaderName, HeaderValue};
use open_ferry_translate::go::to_upper;

use crate::json::eq_fold;

/// The client identity headers no attribute may set, lowercased as
/// [`HeaderName`] keeps them: the client, and the session, conversation,
/// thread, window, agent or container IDs that upstream's executors pass on
/// from Codex and Claude Code clients or make up, and the headers by which
/// Google's API client (`x-goog-api-client`), Meta (`x-client-id`) and xAI
/// (`x-xai-token-auth`) name theirs. Any `x-stainless-*`, `x-msh-*` (Kimi's)
/// or `x-grok-client-*` (xAI's) header counts too.
const IDENTITY_HEADERS: [&str; 18] = [
    "user-agent",
    "x-app",
    "originator",
    "session_id",
    "session-id",
    "conversation_id",
    "conversation-id",
    "thread-id",
    "thread_id",
    "x-codex-window-id",
    "x-claude-code-session-id",
    "x-claude-code-agent-id",
    "x-claude-code-parent-agent-id",
    "x-claude-remote-session-id",
    "x-claude-remote-container-id",
    "x-goog-api-client",
    "x-client-id",
    "x-xai-token-auth",
];

/// The prefixes of the identity header families, lowercased.
const IDENTITY_PREFIXES: [&str; 3] = ["x-stainless-", "x-msh-", "x-grok-client-"];

/// Whether `name` says which client is calling, so no attribute may set it
/// (nor a credential's quota probe).
pub fn is_identity_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    IDENTITY_HEADERS.contains(&name) || IDENTITY_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// The headers that frame a request's body, which Go's HTTP client writes
/// from the body and never from the headers it was given
/// (`reqWriteExcludeHeader`, less the `Host` and `User-Agent` it handles
/// apart).
const FRAMING_HEADERS: [HeaderName; 3] = [
    http::header::CONTENT_LENGTH,
    http::header::TRANSFER_ENCODING,
    http::header::TRAILER,
];

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
        if FRAMING_HEADERS.contains(&header) {
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
            "Conversation_id",
            "CONVERSATION_ID",
            "Conversation-Id",
            "Thread-Id",
            "thread_id",
            "THREAD-ID",
            "X-Codex-Window-Id",
            "x-codex-window-id",
            "X-Claude-Code-Agent-Id",
            "X-Claude-Code-Parent-Agent-Id",
            "X-Claude-Remote-Session-Id",
            "X-CLAUDE-REMOTE-CONTAINER-ID",
            "X-Goog-Api-Client",
            "x-goog-api-client",
            "X-GOOG-API-CLIENT",
            "X-Client-Id",
            "x-client-id",
            "X-CLIENT-ID",
            "X-Xai-Token-Auth",
            "x-xai-token-auth",
            "X-XAI-TOKEN-AUTH",
            "X-Msh-Platform",
            "x-msh-version",
            "X-Msh-Device-Id",
            "X-MSH-DEVICE-NAME",
            "x-msh-anything-else",
            "X-Grok-Client-Version",
            "x-grok-client-identifier",
            "X-GROK-CLIENT-SESSION-ID",
        ] {
            let header = HeaderName::from_bytes(name.as_bytes()).unwrap();
            assert!(is_identity_header(&header), "{name}");
        }
        for name in [
            "X-Team",
            "X-Stainless",
            "Session",
            "X-Session-Id",
            "Conversation",
            "X-Thread",
            "X-App-Version",
            "Anthropic-Beta",
            "Authorization",
            "X-Goog-Api-Key",
            "X-Goog-User-Project",
            "X-Client",
            "X-Xai-Token",
            "X-Msh",
            "X-Mshx-Version",
            "X-Grok",
            "X-Grok-Client",
            "X-Grok-Clients",
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
                ("header:Conversation_id", "invented-conversation"),
                ("header:conversation-ID", "$X-Source"),
                ("header:Thread-Id", "invented-thread"),
                ("header:thread_id", "invented-thread"),
                ("header:X-Codex-Window-Id", "invented-window"),
                ("header:X-Claude-Code-Agent-Id", "invented-agent"),
                ("header:X-Claude-Remote-Container-Id", "invented-container"),
                ("header:X-Goog-Api-Client", "gl-node/22 gdcl/9"),
                ("header:x-goog-api-client", "$X-Source"),
                ("header:X-Client-Id", "tbh:tui"),
                ("header:x-client-id", "$X-Source"),
                ("header:X-Xai-Token-Auth", "invented-token"),
                ("header:X-XAI-TOKEN-AUTH", "$X-Source"),
                ("header:X-Msh-Platform", "kimi_cli"),
                ("header:X-Msh-Device-Id", "invented-device"),
                ("header:x-msh-version", "$X-Source"),
                ("header:X-Grok-Client-Version", "9.9.9"),
                ("header:x-grok-client-identifier", "$X-Source"),
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
            "conversation_id",
            "conversation-id",
            "thread-id",
            "thread_id",
            "x-codex-window-id",
            "x-claude-code-agent-id",
            "x-claude-remote-container-id",
            "x-goog-api-client",
            "x-client-id",
            "x-xai-token-auth",
            "x-msh-platform",
            "x-msh-device-id",
            "x-msh-version",
            "x-grok-client-version",
            "x-grok-client-identifier",
        ] {
            assert!(target.get(absent).is_none(), "{absent}");
        }
        assert_eq!(target.get("x-team").unwrap(), "blue");
        assert_eq!(target.get("x-forward").unwrap(), "from-client");
        // Copying the client's own value into another header is fine.
        assert_eq!(target.get("x-agent-copy").unwrap(), "actual-client/1");
    }

    #[test]
    fn skips_the_headers_that_frame_the_body() {
        let mut target = HeaderMap::new();
        apply(
            &mut target,
            &attributes(&[
                ("header:Content-Length", "1"),
                ("header:transfer-encoding", "chunked"),
                ("header:TRAILER", "X-Checksum"),
                ("header:Host", "upstream.example"),
            ]),
            &HeaderMap::new(),
            "test",
        );
        for absent in ["content-length", "transfer-encoding", "trailer"] {
            assert!(target.get(absent).is_none(), "{absent}");
        }
        assert_eq!(target.get("host").unwrap(), "upstream.example");
    }
}
