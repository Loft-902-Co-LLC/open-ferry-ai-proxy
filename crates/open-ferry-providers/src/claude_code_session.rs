// Ported from CLIProxyAPI internal/runtime/executor/helps/claude_code_session.go
// (ExtractClaudeCodeSessionID, ExtractClaudeCodeAgentID,
// ClaudeCodeExecutionScope, headerValueCaseInsensitive) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Claude Code session and agent a request belongs to, as Claude Code
//! sends them.
//!
//! The session comes from the `X-Claude-Code-Session-Id` header, or else
//! from the payload's `metadata.user_id`: either a legacy ID ending in
//! `_session_<hex>`, or a JSON object with a `session_id`. The agent comes
//! from `X-Claude-Code-Agent-Id`, and is `main` for the root agent.
//! [`execution_scope`] joins the two, so that each agent of a session keeps
//! its own state.
//!
//! Deviations from upstream:
//! - `ClaudeCodePromptCache`, which makes a `prompt_cache_key` from the
//!   session, isn't ported: only a key the client sent is passed on.
//! - The headers are the client's headers in the executor's options; upstream
//!   also falls back to the gin request's, which are the same headers here.
//! - A `metadata.user_id` holding JSON is parsed as JSON: if it is malformed
//!   there is no session, where gjson reads what it can, and a repeated
//!   `session_id` gives its last value, where gjson gives the first. A payload
//!   that isn't JSON gives no session either.
//! - `HeaderValueCaseInsensitive` and `HeaderValuesCaseInsensitive` aren't
//!   exported: nothing else uses them here.

use http::HeaderMap;
use serde::Deserialize;
use serde_json::Value;

use crate::json::str_of;

/// The header Claude Code names its session in.
pub(crate) const SESSION_HEADER: &str = "X-Claude-Code-Session-Id";

/// The header Claude Code names a subagent in.
pub(crate) const AGENT_HEADER: &str = "X-Claude-Code-Agent-Id";

/// The agent of a request that names none: Claude Code's root agent.
pub(crate) const MAIN_AGENT: &str = "main";

/// The suffix of a legacy `metadata.user_id` before its session ID.
const SESSION_SUFFIX: &str = "_session_";

/// The part of a Claude payload read here.
#[derive(Deserialize)]
struct Payload {
    #[serde(default)]
    metadata: Value,
}

/// The session ID: the session header, or else the payload's
/// `metadata.user_id` (`ExtractClaudeCodeSessionID`). `""` if neither has
/// one.
pub(crate) fn session_id(payload: &[u8], headers: &HeaderMap) -> String {
    let from_header = header_value(headers, SESSION_HEADER);
    if !from_header.is_empty() {
        return from_header;
    }
    session_id_from_payload(payload)
}

/// The agent ID, or [`MAIN_AGENT`] (`ExtractClaudeCodeAgentID`).
pub(crate) fn agent_id(headers: &HeaderMap) -> String {
    let agent = header_value(headers, AGENT_HEADER);
    if agent.is_empty() {
        MAIN_AGENT.to_owned()
    } else {
        agent
    }
}

/// `claude:<session>:agent:<agent>`, if the request names a session
/// (`ClaudeCodeExecutionScope`).
pub(crate) fn execution_scope(payload: &[u8], headers: &HeaderMap) -> Option<String> {
    let session = session_id(payload, headers);
    if session.is_empty() {
        return None;
    }
    Some(format!("claude:{session}:agent:{}", agent_id(headers)))
}

/// The first of the header's values that isn't blank, trimmed, or `""`
/// (`headerValueCaseInsensitive`). Header names don't depend on case here.
pub(crate) fn header_value(headers: &HeaderMap, name: &str) -> String {
    headers
        .get_all(name)
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).trim().to_owned())
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

/// `extractClaudeCodeSessionIDFromPayload`.
fn session_id_from_payload(payload: &[u8]) -> String {
    if payload.is_empty() {
        return String::new();
    }
    let Ok(payload) = serde_json::from_slice::<Payload>(payload) else {
        return String::new();
    };
    let user_id = str_of(payload.metadata.get("user_id"));
    session_id_from_user_id(&user_id)
}

/// The session ID in a `metadata.user_id`: what follows a final
/// `_session_` if it is all lowercase hex digits and dashes (upstream's
/// `_session_([a-f0-9-]+)$`), or else the `session_id` of a JSON object.
fn session_id_from_user_id(user_id: &str) -> String {
    if let Some(start) = user_id.rfind(SESSION_SUFFIX) {
        let id = &user_id[start + SESSION_SUFFIX.len()..];
        if !id.is_empty()
            && id
                .bytes()
                .all(|byte| matches!(byte, b'a'..=b'f' | b'0'..=b'9' | b'-'))
        {
            return id.to_owned();
        }
    }
    if !user_id.starts_with('{') {
        return String::new();
    }
    match serde_json::from_str::<Value>(user_id) {
        Ok(object) => str_of(object.get("session_id")).trim().to_owned(),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    //! Ported from upstream's `helps/claude_code_session_test.go`.
    //!
    //! Dropped: `TestClaudeCodePromptCacheStableAcrossRequests` and
    //! `TestClaudeCodePromptCacheDeterministicAndAgentScoped`, as
    //! `ClaudeCodePromptCache` isn't ported. Changed:
    //! `TestExtractClaudeCodeSessionIDFromHeader` passes the header in the
    //! headers rather than a gin context, which is where the client's headers
    //! are here.

    use http::{HeaderName, HeaderValue};

    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn session_id_from_payload_json() {
        let payload = br#"{"metadata":{"user_id":"{\"device_id\":\"d\",\"session_id\":\"cache-session-1\"}"}}"#;
        assert_eq!(session_id(payload, &HeaderMap::new()), "cache-session-1");
    }

    #[test]
    fn session_id_from_header() {
        let headers = headers(&[(SESSION_HEADER, "header-session-1")]);
        assert_eq!(
            session_id(br#"{"model":"gpt-5.4"}"#, &headers),
            "header-session-1"
        );
    }

    #[test]
    fn session_id_prefers_header_over_payload() {
        let payload = br#"{"metadata":{"user_id":"{"session_id":"payload-session"}"}}"#;
        let headers = headers(&[(SESSION_HEADER, "header-session")]);
        assert_eq!(session_id(payload, &headers), "header-session");
    }

    #[test]
    fn execution_scope_accepts_lowercase_header_names() {
        let headers = headers(&[
            ("x-claude-code-session-id", "lower-session"),
            ("x-claude-code-agent-id", "lower-agent"),
        ]);
        assert_eq!(
            execution_scope(&[], &headers).as_deref(),
            Some("claude:lower-session:agent:lower-agent")
        );
    }

    #[test]
    fn execution_scope_isolates_agents() {
        let root = headers(&[(SESSION_HEADER, "session-agents")]);
        let child_a = headers(&[
            (SESSION_HEADER, "session-agents"),
            (AGENT_HEADER, "agent-a"),
        ]);
        let child_b = headers(&[
            (SESSION_HEADER, "session-agents"),
            (AGENT_HEADER, "agent-b"),
        ]);

        let root = execution_scope(&[], &root).unwrap();
        let child_a = execution_scope(&[], &child_a).unwrap();
        let child_b = execution_scope(&[], &child_b).unwrap();
        assert_eq!(root, "claude:session-agents:agent:main");
        assert_eq!(child_a, "claude:session-agents:agent:agent-a");
        assert_eq!(child_b, "claude:session-agents:agent:agent-b");
        assert!(root != child_a && child_a != child_b && root != child_b);
    }

    // Not upstream's: the legacy user ID, blank headers, and a user ID that
    // is neither form.
    #[test]
    fn session_id_from_legacy_user_id_and_blank_values() {
        let legacy = br#"{"metadata":{"user_id":"user_abc_account__session_0a1b-2c3d"}}"#;
        assert_eq!(session_id(legacy, &HeaderMap::new()), "0a1b-2c3d");
        let upper = br#"{"metadata":{"user_id":"user_abc_session_0A1B"}}"#;
        assert_eq!(session_id(upper, &HeaderMap::new()), "");
        let bare = br#"{"metadata":{"user_id":"same-user-across-chats"}}"#;
        assert_eq!(session_id(bare, &HeaderMap::new()), "");
        assert_eq!(execution_scope(bare, &HeaderMap::new()), None);

        let blank = headers(&[(SESSION_HEADER, "  "), (SESSION_HEADER, " second ")]);
        assert_eq!(session_id(b"", &blank), "second");
        let blank_agent = headers(&[(SESSION_HEADER, "s"), (AGENT_HEADER, " ")]);
        assert_eq!(
            execution_scope(b"", &blank_agent).as_deref(),
            Some("claude:s:agent:main")
        );
    }
}
