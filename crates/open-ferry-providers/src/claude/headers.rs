// Ported from CLIProxyAPI internal/runtime/executor/claude_executor_request.go
// (applyClaudeHeadersWithNativeProfile on its caller-owned path, the beta
// helpers and copyClaudeCallerFingerprintHeaders), internal/util/header_helpers.go,
// internal/misc/header_utils.go and helps/claude_code_session.go
// (HeaderValuesCaseInsensitive) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The headers of a request to Claude.
//!
//! The key goes in `x-api-key` for an API key on Anthropic's own API and in a
//! Bearer header otherwise. `anthropic-beta` is the client's betas, the
//! body's `betas`, `oauth-2025-04-20` for an OAuth token and the betas a
//! request needs for what it asks for (the advisor tool, fast mode, a
//! one-hour cache). The client's own `Accept`, `User-Agent`, `anthropic-*`
//! and a few other headers pass through as upstream passes them; otherwise
//! `anthropic-version` is `2023-06-01` and `User-Agent` is
//! `open-ferry/<version>`. A credential's `header:<Name>` attributes come
//! last.
//!
//! Deviations from upstream:
//! - Only upstream's path for a client it doesn't take for Claude Code is
//!   ported. The Claude Code profile it applies to OAuth tokens and
//!   configured credentials (a fixed beta list, `x-app`, `x-stainless-*`,
//!   `claude-cli` user agents, device profiles, session IDs) is client
//!   impersonation and is left out, as is detecting Claude Code itself, so
//!   the client's `x-claude-code-*` and `x-claude-remote-*` headers are
//!   never forwarded.
//! - `oauth-2025-04-20` is added for an OAuth token alone, where upstream
//!   ties it to that profile.
//! - Without a client `User-Agent` the request identifies as
//!   `open-ferry/<version>` rather than `CLIProxyAPI/<version>`.
//! - No `Accept-Encoding` is sent, and the client's isn't forwarded: the
//!   executor reads uncompressed bodies only.
//! - `X-Claude-Code-Session-Id` is never set, and a custom header whose value
//!   names `$CPA-SESSION-ID` is skipped, as there are no session IDs.
//! - The beta removals that copy Claude Code's gating (effort on models
//!   without it, display updates, server-side fallback on Haiku, probe and
//!   subagent heuristics) aren't ported: the client's betas stay as sent.
//! - The executor's own base URL counts as Anthropic's even when it isn't,
//!   so tests can point it at a mock server.

use std::collections::BTreeMap;

use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use super::client;
use super::json;
use super::request::{
    ADVISOR_TOOL_BETA, AFTER_ADVISOR_BETAS, CLAUDE_CODE_BETA, EXTENDED_CACHE_TTL_BETA,
    FAST_MODE_BETA, OAUTH_BETA, is_oauth_token, payload_has_1h_ttl,
};

const ANTHROPIC_BETA: &str = "anthropic-beta";
const ANTHROPIC_VERSION: &str = "anthropic-version";
const ACCEPT: &str = "accept";
const USER_AGENT: &str = "user-agent";

/// What the headers depend on.
pub(crate) struct Inputs<'a> {
    /// The API key or OAuth access token.
    pub key: &'a str,
    /// Whether the key goes in a Bearer header (`claudeCredentialUsesOAuth`).
    pub bearer: bool,
    /// Whether the request goes to Anthropic's own API.
    pub first_party: bool,
    pub stream: bool,
    pub count_tokens: bool,
    /// Betas lifted from the body.
    pub extra_betas: &'a [String],
    pub body: &'a Value,
    /// The client's request headers.
    pub client: &'a HeaderMap,
    /// The credential's attributes, for `header:<Name>` overrides.
    pub attributes: &'a BTreeMap<String, String>,
}

/// Builds the request headers.
pub(crate) fn build(inputs: &Inputs<'_>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if !inputs.key.trim().is_empty() {
        let (name, value) = if inputs.first_party && !inputs.bearer {
            ("x-api-key", inputs.key.to_owned())
        } else {
            ("authorization", format!("Bearer {}", inputs.key))
        };
        match HeaderValue::from_str(&value) {
            Ok(mut value) => {
                value.set_sensitive(true);
                headers.insert(name, value);
            }
            Err(_) => {
                tracing::warn!(
                    "claude: the credential's key can't go in a header; sent without it"
                );
            }
        }
    }
    headers.insert("content-type", HeaderValue::from_static("application/json"));

    let betas = betas(inputs);
    apply_betas(&mut headers, &betas);

    let default_accept = if inputs.stream && !inputs.first_party {
        "text/event-stream"
    } else {
        "application/json"
    };
    copy_client_headers(&mut headers, inputs.client);
    ensure_header(&mut headers, inputs.client, ANTHROPIC_VERSION, "2023-06-01");
    ensure_header(&mut headers, inputs.client, ACCEPT, default_accept);
    ensure_header(&mut headers, inputs.client, USER_AGENT, client::USER_AGENT);
    apply_betas(&mut headers, &betas);
    apply_custom_headers(&mut headers, inputs.attributes, inputs.client);
    if inputs.first_party {
        apply_betas(&mut headers, &betas);
        reset_header(&mut headers, inputs.client, ACCEPT, default_accept);
    } else if inputs.stream {
        reset_header(&mut headers, inputs.client, ACCEPT, default_accept);
    }
    headers
}

/// The `anthropic-beta` value.
fn betas(inputs: &Inputs<'_>) -> String {
    let incoming = header_values(inputs.client, ANTHROPIC_BETA).join(",");
    let incoming = incoming.trim();
    let advisor_needed = split(incoming)
        .chain(inputs.extra_betas.iter().map(|beta| beta.trim()))
        .any(|beta| beta == ADVISOR_TOOL_BETA)
        || body_has_advisor_tool(inputs.body);

    let mut betas = incoming.to_owned();
    if is_oauth_token(inputs.key) {
        betas = with_oauth_beta(&betas);
    }
    if advisor_needed {
        betas = with_advisor_tool_beta(&betas);
    }

    let mut existing: Vec<String> = split(&betas).map(str::to_owned).collect();
    let mut append = |beta: &str| {
        let beta = beta.trim();
        if beta.is_empty() || existing.iter().any(|known| known == beta) {
            return;
        }
        if betas.trim().is_empty() {
            betas = beta.to_owned();
        } else {
            betas.push(',');
            betas.push_str(beta);
        }
        existing.push(beta.to_owned());
    };
    if json::eq_fold(json::str_at(inputs.body, "speed").trim(), "fast") {
        append(FAST_MODE_BETA);
    }
    for beta in inputs.extra_betas {
        append(beta);
    }
    if !inputs.count_tokens && payload_has_1h_ttl(inputs.body) {
        betas = with_extended_cache_ttl_beta(&betas);
    }
    betas
}

/// The non-empty, trimmed entries of a comma-separated list.
fn split(list: &str) -> impl Iterator<Item = &str> {
    list.split(',')
        .map(str::trim)
        .filter(|beta| !beta.is_empty())
}

/// The list with duplicates removed.
fn dedupe<'a>(list: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut parts: Vec<&str> = Vec::new();
    for beta in list {
        if !parts.contains(&beta) {
            parts.push(beta);
        }
    }
    parts
}

/// `withClaudeOAuthCredentialBetas` without the cache beta, which is also
/// `withClaudeCountTokensOAuthBeta`: an OAuth token must declare
/// `oauth-2025-04-20`. It goes first, or after a leading
/// `claude-code-20250219`.
fn with_oauth_beta(betas: &str) -> String {
    let mut parts = dedupe(split(betas));
    if !parts.contains(&OAUTH_BETA) {
        let at = usize::from(parts.first() == Some(&CLAUDE_CODE_BETA));
        parts.insert(at, OAUTH_BETA);
    }
    parts.join(",")
}

/// `withClaudeAdvisorToolBeta`: the advisor tool needs its beta, which goes
/// before the first of the betas Anthropic lists after it.
fn with_advisor_tool_beta(betas: &str) -> String {
    if betas.trim().is_empty() {
        return ADVISOR_TOOL_BETA.to_owned();
    }
    let mut parts = dedupe(split(betas).filter(|beta| *beta != ADVISOR_TOOL_BETA));
    let at = parts
        .iter()
        .position(|beta| AFTER_ADVISOR_BETAS.contains(beta))
        .unwrap_or(parts.len());
    parts.insert(at, ADVISOR_TOOL_BETA);
    parts.join(",")
}

/// `withClaudeExtendedCacheTTLBeta`: a one-hour breakpoint needs the
/// extended cache beta.
fn with_extended_cache_ttl_beta(betas: &str) -> String {
    let mut parts = dedupe(split(betas));
    if !parts.contains(&EXTENDED_CACHE_TTL_BETA) {
        parts.push(EXTENDED_CACHE_TTL_BETA);
    }
    parts.join(",")
}

/// `claudeBodyHasAdvisorTool`.
fn body_has_advisor_tool(body: &Value) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| json::lower_trim(&json::str_at(tool, "type")).starts_with("advisor_"))
        })
}

/// Sets `anthropic-beta`, or removes it when there are none.
fn apply_betas(headers: &mut HeaderMap, betas: &str) {
    match HeaderValue::from_bytes(betas.as_bytes()) {
        Ok(value) if !betas.trim().is_empty() => {
            headers.insert(ANTHROPIC_BETA, value);
        }
        _ => {
            headers.remove(ANTHROPIC_BETA);
        }
    }
}

/// `HeaderValuesCaseInsensitive`: the header's non-empty values, trimmed.
fn header_values(headers: &HeaderMap, name: &str) -> Vec<String> {
    headers
        .get_all(name)
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect()
}

/// `copyClaudeCallerFingerprintHeaders` for a client upstream doesn't take
/// for Claude Code: the client's own `Accept`, `User-Agent`, `x-app`,
/// `x-client-request-id`, `x-client-app`, `x-stainless-*`, `anthropic-*` and
/// `x-anthropic-additional-protection` headers replace ours.
/// `Accept-Encoding` stays behind; see the module docs.
fn copy_client_headers(headers: &mut HeaderMap, client: &HeaderMap) {
    for name in client.keys() {
        let lower = name.as_str();
        let forwarded = matches!(
            lower,
            "accept"
                | "user-agent"
                | "x-app"
                | "x-client-request-id"
                | "x-client-app"
                | "x-anthropic-additional-protection"
        ) || lower.starts_with("anthropic-")
            || lower.starts_with("x-stainless-");
        if !forwarded {
            continue;
        }
        headers.remove(name);
        for value in client.get_all(name) {
            headers.append(name.clone(), value.clone());
        }
    }
}

/// `misc.EnsureHeader`: the client's value, else what is set, else the
/// default.
fn ensure_header(
    headers: &mut HeaderMap,
    client: &HeaderMap,
    name: &'static str,
    default: &'static str,
) {
    if let Some(value) = trimmed(client, name) {
        headers.insert(name, value);
        return;
    }
    if trimmed(headers, name).is_some() {
        return;
    }
    headers.insert(name, HeaderValue::from_static(default));
}

/// The client's value, else the default, over anything set.
fn reset_header(
    headers: &mut HeaderMap,
    client: &HeaderMap,
    name: &'static str,
    default: &'static str,
) {
    let value = trimmed(client, name).unwrap_or(HeaderValue::from_static(default));
    headers.insert(name, value);
}

/// The first value of a header, trimmed, if it isn't blank.
fn trimmed(headers: &HeaderMap, name: &str) -> Option<HeaderValue> {
    let value = headers.get(name)?;
    let text = String::from_utf8_lossy(value.as_bytes());
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    HeaderValue::from_bytes(text.as_bytes()).ok()
}

/// `ApplyCustomHeadersFromAttrs`: the credential's `header:<Name>`
/// attributes override what is set. A value of `$Name` takes the client's
/// `Name` header, and is skipped when the client sent none.
fn apply_custom_headers(
    target: &mut HeaderMap,
    attributes: &BTreeMap<String, String>,
    client: &HeaderMap,
) {
    for (key, value) in attributes {
        let Some(name) = key.strip_prefix("header:") else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim());
        if name.is_empty() || value.is_empty() {
            continue;
        }
        let value: &[u8] = match value.strip_prefix('$') {
            Some(variable) if json::eq_fold(variable.trim(), "CPA-SESSION-ID") => continue,
            _ if value.to_uppercase().contains("$CPA-SESSION-ID") => continue,
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
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_bytes(value),
        ) {
            (Ok(name), Ok(value)) => {
                target.insert(name, value);
            }
            _ => tracing::warn!(
                "claude: custom header attribute {key:?} isn't a valid HTTP header; skipped"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Case {
        key: &'static str,
        bearer: bool,
        first_party: bool,
        stream: bool,
        count_tokens: bool,
        extra: Vec<String>,
        body: Value,
        client: Vec<(&'static str, &'static str)>,
        attributes: Vec<(&'static str, &'static str)>,
    }

    impl Default for Case {
        fn default() -> Self {
            Case {
                key: "key",
                bearer: false,
                first_party: true,
                stream: false,
                count_tokens: false,
                extra: Vec::new(),
                body: json!({"model": "claude-opus-5"}),
                client: Vec::new(),
                attributes: Vec::new(),
            }
        }
    }

    impl Case {
        fn build(&self) -> HeaderMap {
            let mut client = HeaderMap::new();
            for (name, value) in &self.client {
                client.append(
                    HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    HeaderValue::from_static(value),
                );
            }
            let attributes = self
                .attributes
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect();
            build(&Inputs {
                key: self.key,
                bearer: self.bearer,
                first_party: self.first_party,
                stream: self.stream,
                count_tokens: self.count_tokens,
                extra_betas: &self.extra,
                body: &self.body,
                client: &client,
                attributes: &attributes,
            })
        }
    }

    fn get<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
        headers.get(name).map(|value| value.to_str().unwrap())
    }

    #[test]
    fn defaults() {
        let headers = Case::default().build();
        assert_eq!(get(&headers, "x-api-key"), Some("key"));
        assert_eq!(get(&headers, "authorization"), None);
        assert_eq!(get(&headers, "content-type"), Some("application/json"));
        assert_eq!(get(&headers, "anthropic-version"), Some("2023-06-01"));
        assert_eq!(get(&headers, "accept"), Some("application/json"));
        assert_eq!(get(&headers, "user-agent"), Some(client::USER_AGENT));
        assert!(client::USER_AGENT.starts_with("open-ferry/"));
        for absent in [
            "anthropic-beta",
            "accept-encoding",
            "x-app",
            "x-stainless-lang",
            "anthropic-dangerous-direct-browser-access",
            "x-claude-code-session-id",
        ] {
            assert_eq!(get(&headers, absent), None, "{absent}");
        }
        assert!(headers.get("x-api-key").unwrap().is_sensitive());

        // A gateway gets the key as a Bearer and streams as SSE.
        let headers = Case {
            first_party: false,
            stream: true,
            ..Case::default()
        }
        .build();
        assert_eq!(get(&headers, "authorization"), Some("Bearer key"));
        assert_eq!(get(&headers, "x-api-key"), None);
        assert_eq!(get(&headers, "accept"), Some("text/event-stream"));
    }

    // TestApplyClaudeHeaders_EmptyAPIKey_OmitsAuthHeaders.
    #[test]
    fn empty_key_sends_no_auth() {
        let headers = Case {
            key: " ",
            first_party: false,
            attributes: vec![("header:Custom-Token", "custom-secret")],
            ..Case::default()
        }
        .build();
        assert_eq!(get(&headers, "authorization"), None);
        assert_eq!(get(&headers, "x-api-key"), None);
        assert_eq!(get(&headers, "custom-token"), Some("custom-secret"));
    }

    #[test]
    fn oauth_tokens_declare_the_oauth_beta() {
        let headers = Case {
            key: "sk-ant-oat01-x",
            bearer: true,
            ..Case::default()
        }
        .build();
        assert_eq!(
            get(&headers, "authorization"),
            Some("Bearer sk-ant-oat01-x")
        );
        assert_eq!(get(&headers, "anthropic-beta"), Some(OAUTH_BETA));

        let headers = Case {
            key: "sk-ant-oat01-x",
            bearer: true,
            client: vec![
                (
                    "anthropic-beta",
                    "claude-code-20250219, interleaved-thinking-2025-05-14",
                ),
                ("anthropic-beta", "claude-code-20250219"),
            ],
            ..Case::default()
        }
        .build();
        assert_eq!(
            get(&headers, "anthropic-beta"),
            Some("claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14")
        );

        // An API key never gets it, and the client's header stays as sent.
        let headers = Case {
            client: vec![("anthropic-beta", "a, b")],
            ..Case::default()
        }
        .build();
        assert_eq!(get(&headers, "anthropic-beta"), Some("a, b"));
    }

    // TestApplyClaudeHeaders_AdvisorToolBetaInjectedWhenBodyHasTool_APIKeyPassthrough
    // and TestApplyClaudeHeaders_AdvisorToolBetaRepositionedWhenOutOfOrder.
    #[test]
    fn advisor_tool_beta() {
        for stream in [false, true] {
            let headers = Case {
                stream,
                body: json!({"model": "claude-opus-5", "tools": [{"type": "advisor_20260301", "name": "advisor"}]}),
                client: vec![(
                    "anthropic-beta",
                    "claude-code-20250219,mid-conversation-system-2026-04-07,advanced-tool-use-2025-11-20,effort-2025-11-24",
                )],
                ..Case::default()
            }
            .build();
            assert_eq!(
                get(&headers, "anthropic-beta"),
                Some(
                    "claude-code-20250219,mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,effort-2025-11-24"
                )
            );
        }
        let headers = Case {
            client: vec![(
                "anthropic-beta",
                "effort-2025-11-24,advisor-tool-2026-03-01",
            )],
            ..Case::default()
        }
        .build();
        assert_eq!(
            get(&headers, "anthropic-beta"),
            Some("advisor-tool-2026-03-01,effort-2025-11-24")
        );
        let headers = Case {
            extra: vec![ADVISOR_TOOL_BETA.into()],
            ..Case::default()
        }
        .build();
        assert_eq!(get(&headers, "anthropic-beta"), Some(ADVISOR_TOOL_BETA));
    }

    // The caller-owned cases of TestApplyClaudeHeaders_FastModeBetaIsConditional.
    #[test]
    fn feature_betas() {
        let betas = |case: Case| get(&case.build(), "anthropic-beta").map(str::to_owned);
        assert_eq!(betas(Case::default()), None);
        assert_eq!(
            betas(Case {
                body: json!({"speed": " FAST "}),
                ..Case::default()
            })
            .as_deref(),
            Some(FAST_MODE_BETA)
        );
        assert_eq!(
            betas(Case {
                extra: vec![FAST_MODE_BETA.into(), " x ".into()],
                client: vec![("anthropic-beta", "x")],
                ..Case::default()
            })
            .as_deref(),
            Some("x,fast-mode-2026-02-01")
        );
        let one_hour = json!({"system": [{"type": "text", "text": "s", "cache_control": {"type": "ephemeral", "ttl": "1h"}}]});
        assert_eq!(
            betas(Case {
                body: one_hour.clone(),
                client: vec![("anthropic-beta", "a, a")],
                ..Case::default()
            })
            .as_deref(),
            Some("a,extended-cache-ttl-2025-04-11")
        );
        assert_eq!(
            betas(Case {
                body: one_hour,
                count_tokens: true,
                ..Case::default()
            }),
            None
        );
    }

    // TestApplyClaudeHeadersPreservesCallerAsyncWithoutFingerprintOptIn and
    // TestApplyClaudeHeaders_PreservesNativeGatewayHintsOnly, for a client
    // that isn't taken for Claude Code.
    #[test]
    fn client_headers_pass_through() {
        let headers = Case {
            stream: true,
            client: vec![
                ("x-stainless-async", "async"),
                ("x-app", "cli"),
                ("user-agent", " my-client/1.0 "),
                ("accept", "text/event-stream"),
                ("accept-encoding", "gzip"),
                ("anthropic-version", "2024-01-01"),
                ("anthropic-dangerous-direct-browser-access", "true"),
                ("x-client-request-id", "r1"),
                ("x-claude-code-session-id", "s1"),
                ("x-claude-code-request-class", "main"),
                ("x-claude-remote-container-id", "c1"),
                ("x-api-key", "client-key"),
                ("authorization", "Bearer client"),
                ("x-other", "o"),
            ],
            ..Case::default()
        }
        .build();
        assert_eq!(get(&headers, "x-stainless-async"), Some("async"));
        assert_eq!(get(&headers, "x-app"), Some("cli"));
        assert_eq!(get(&headers, "user-agent"), Some("my-client/1.0"));
        assert_eq!(get(&headers, "accept"), Some("text/event-stream"));
        assert_eq!(get(&headers, "anthropic-version"), Some("2024-01-01"));
        assert_eq!(
            get(&headers, "anthropic-dangerous-direct-browser-access"),
            Some("true")
        );
        assert_eq!(get(&headers, "x-client-request-id"), Some("r1"));
        assert_eq!(get(&headers, "x-api-key"), Some("key"));
        for absent in [
            "accept-encoding",
            "x-claude-code-session-id",
            "x-claude-code-request-class",
            "x-claude-remote-container-id",
            "authorization",
            "x-other",
            "anthropic-beta",
        ] {
            assert_eq!(get(&headers, absent), None, "{absent}");
        }
    }

    // TestApplyClaudeHeaders_CustomHeadersCannotOverrideAnthropicIdentity, as
    // far as it applies: on Anthropic's API the betas and Accept are restored
    // after custom headers; elsewhere they stay unless streaming.
    #[test]
    fn custom_headers() {
        let attributes = vec![
            ("header:Anthropic-Beta", "custom-beta"),
            ("header:Accept", "text/plain"),
            ("header:X-Team", "blue"),
            ("header:X-Forward", "$X-Source"),
            ("header:X-Missing", "$X-Absent"),
            ("header:X-Session", "$CPA-SESSION-ID"),
            ("header:X-Session-2", "id=$cpa-session-id"),
            ("header:", "x"),
        ];
        let client = vec![("x-source", "from-client"), ("anthropic-beta", "b1")];
        let headers = Case {
            attributes: attributes.clone(),
            client: client.clone(),
            ..Case::default()
        }
        .build();
        assert_eq!(get(&headers, "anthropic-beta"), Some("b1"));
        assert_eq!(get(&headers, "accept"), Some("application/json"));
        assert_eq!(get(&headers, "x-team"), Some("blue"));
        assert_eq!(get(&headers, "x-forward"), Some("from-client"));
        assert_eq!(get(&headers, "x-missing"), None);
        assert_eq!(get(&headers, "x-session"), None);
        assert_eq!(get(&headers, "x-session-2"), None);

        let headers = Case {
            first_party: false,
            attributes: attributes.clone(),
            client: client.clone(),
            ..Case::default()
        }
        .build();
        assert_eq!(get(&headers, "anthropic-beta"), Some("custom-beta"));
        assert_eq!(get(&headers, "accept"), Some("text/plain"));

        let headers = Case {
            first_party: false,
            stream: true,
            attributes,
            client,
            ..Case::default()
        }
        .build();
        assert_eq!(get(&headers, "anthropic-beta"), Some("custom-beta"));
        assert_eq!(get(&headers, "accept"), Some("text/event-stream"));
    }

    #[test]
    fn beta_helpers() {
        assert_eq!(with_oauth_beta(""), OAUTH_BETA);
        assert_eq!(
            with_oauth_beta("a,oauth-2025-04-20,a"),
            "a,oauth-2025-04-20"
        );
        assert_eq!(with_advisor_tool_beta(" "), ADVISOR_TOOL_BETA);
        assert_eq!(
            with_advisor_tool_beta("a,cache-diagnosis-2026-04-07,fast-mode-2026-02-01"),
            "a,advisor-tool-2026-03-01,cache-diagnosis-2026-04-07,fast-mode-2026-02-01"
        );
        assert_eq!(with_advisor_tool_beta("a,b"), "a,b,advisor-tool-2026-03-01");
        assert!(body_has_advisor_tool(
            &json!({"tools": [{"type": " Advisor_x "}]})
        ));
        assert!(!body_has_advisor_tool(
            &json!({"tools": {"type": "advisor_x"}})
        ));
    }
}
