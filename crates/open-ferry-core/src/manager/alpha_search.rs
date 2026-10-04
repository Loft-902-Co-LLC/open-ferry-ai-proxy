// Ported from CLIProxyAPI internal/api/server_routes.go (codexAlphaSearch,
// sanitizeCodexAlphaSearchBody, rewriteCodexAlphaSearchModel) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Codex Alpha Search, the manager's [`Dispatcher::codex_alpha_search`].
//!
//! The client's payload goes to Codex untranslated, without the Responses
//! fields search refuses (`prompt_cache_key` and `prompt_cache_retention`).
//! Its `model`, read as Go's decoder reads it (see [`routing`]), is the
//! route model for picking a credential the `codex_alpha_search_v1` policy
//! allows (see [`super::policy`]). A ChatGPT
//! sign-in sends the payload to the Codex executor's base URL plus
//! `/alpha/search`, which is
//! `https://chatgpt.com/backend-api/codex/alpha/search`. An API key that
//! opted in sends it to its own `base_url` plus `/alpha/search`, with the
//! `model` its prefix and aliases resolve to, and fails closed with 503
//! when it has no `base_url`. The answer comes back whatever its status, and
//! nothing about it is recorded on the credential.
//!
//! The request carries `Content-Type` and `Accept: application/json`, the
//! client's own `Version`, `User-Agent`, `Session_id` and
//! `X-Client-Request-Id`, the credential's `account_id` as
//! `ChatGPT-Account-ID`, and what the executor adds: the credential's token
//! and custom headers.
//!
//! Deviations from upstream:
//! - `Originator: codex_cli_rs` isn't sent, by policy: nothing makes the
//!   request pass for Codex CLI's. A client that sends no `User-Agent` gets
//!   the executor's `open-ferry/<version>`.
//! - The payload's `id` doesn't become a session ID, and the call isn't
//!   logged or traced: session affinity and request logging aren't ported.
//! - The Home dispatcher and the plugin model router aren't ported, so the
//!   payload's `model` is the route model as it is.
//! - A changed payload has its top-level keys sorted and `<`, `>`, `&`,
//!   U+2028 and U+2029 escaped, as Go writes it, but nested values are
//!   written as serde_json writes them, where Go copies them as the client
//!   wrote them, without spaces. A model that only differs in how it is
//!   escaped isn't rewritten.
//! - A payload `serde_json` can't read, though Go's decoder can (with
//!   invalid UTF-8, a lone surrogate escape, or nested more than 128 deep),
//!   goes out as it came, with its Responses fields and its model as the
//!   client wrote them. Its `model` still picks the credential.
//! - The `account_id` is sent trimmed, as Go writes header values, and
//!   isn't sent when it can't be a header value.
//!
//! [`Dispatcher::codex_alpha_search`]: crate::exec::Dispatcher::codex_alpha_search

use std::fmt::Write as _;

use bytes::Bytes;
use http::Method;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use open_ferry_translate::go::trim_space;
use serde_json::{Map, Value};

use self::routing::payload_model;
use super::Manager;
use super::credential::attribute;
use super::policy::CredentialPolicy;
use crate::auth::{Auth, AuthKind};
use crate::exec::{AlphaSearch, ErrorKind, ExecError, HttpCall, HttpReply, HttpTarget};

mod routing;

/// The search endpoint, under a ChatGPT sign-in's base URL or an API key's
/// `base_url`.
const SEARCH_PATH: &str = "/alpha/search";
/// How much of the answer's body is read (32 MiB).
const MAX_RESPONSE_BODY: usize = 32 << 20;
/// Responses fields the search endpoint refuses.
const RESPONSES_ONLY_FIELDS: [&str; 2] = ["prompt_cache_key", "prompt_cache_retention"];
/// The client's headers that go along, when not empty.
const CLIENT_HEADERS: [&str; 4] = ["version", "user-agent", "session_id", "x-client-request-id"];
/// Why an API key without a `base_url` can't search.
const MISSING_BASE_URL: &str = "Codex Alpha Search API key base URL unavailable";

impl Manager {
    /// Sends a Codex Alpha Search call with a credential the policy allows.
    /// An error carries the status to answer with: the selection's, else
    /// 503, or the send's, else 502.
    pub(super) async fn alpha_search(&self, request: AlphaSearch) -> Result<HttpReply, ExecError> {
        let route_model = payload_model(&request.body);
        let body = sanitize_body(request.body);
        let picked = self
            .select_auth_with_credential_policy(
                "codex",
                &route_model,
                CredentialPolicy::CodexAlphaSearchV1,
            )
            .map_err(|error| or_status(error, 503))?;
        let auth = picked.auth;
        let mut headers = base_headers(&request.headers);
        if let Some(account) = account_id(&auth) {
            headers.insert(HeaderName::from_static("chatgpt-account-id"), account);
        }
        let (target, body) = if auth.auth_kind() == Some(AuthKind::ApiKey) {
            let base_url = attribute(&auth, "base_url");
            if base_url.is_empty() {
                return Err(ExecError::new(ErrorKind::Upstream, MISSING_BASE_URL).with_status(503));
            }
            let url = format!("{}{SEARCH_PATH}", base_url.trim_end_matches('/'));
            let upstream_model = self.resolve_execution_model(&auth, &route_model);
            let body = if upstream_model.is_empty() {
                body
            } else {
                rewrite_model(body, &upstream_model)
            };
            (HttpTarget::Url(url), body)
        } else {
            (HttpTarget::Path(SEARCH_PATH.to_owned()), body)
        };
        let call = HttpCall {
            method: Method::POST,
            target,
            headers,
            body,
            client_headers: request.headers,
            response_limit: MAX_RESPONSE_BODY,
        };
        picked
            .executor
            .http_request(auth, call)
            .await
            .map_err(|error| or_status(error, 502))
    }
}

/// `error`, with `status` unless it has one (`HTTPStatusFromErrorOr`).
fn or_status(error: ExecError, status: u16) -> ExecError {
    if error.http_status() > 0 {
        error
    } else {
        error.with_status(status)
    }
}

/// The request headers every credential gets: JSON both ways, and the
/// client's own headers that go along, trimmed.
fn base_headers(client: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
    for name in CLIENT_HEADERS {
        let Some(value) = client.get(name) else {
            continue;
        };
        let trimmed = trim_space(value.as_bytes());
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(value) = HeaderValue::from_bytes(trimmed) {
            headers.insert(HeaderName::from_static(name), value);
        }
    }
    headers
}

/// The credential's own ChatGPT account, when it has one.
fn account_id(auth: &Auth) -> Option<HeaderValue> {
    let Some(Value::String(account)) = auth.metadata.get("account_id") else {
        return None;
    };
    let account = account.trim();
    if account.is_empty() {
        return None;
    }
    match HeaderValue::from_str(account) {
        Ok(value) => Some(value),
        Err(_) => {
            tracing::warn!(
                "codex alpha search: the credential's account_id isn't a valid header value; not sent"
            );
            None
        }
    }
}

/// The payload as a JSON object, unless it isn't one.
fn parse_object(raw: &[u8]) -> Option<Map<String, Value>> {
    match serde_json::from_slice(raw) {
        Ok(Value::Object(object)) => Some(object),
        _ => None,
    }
}

/// The payload without the Responses fields search refuses, or as it is
/// when it has none or isn't a JSON object (`sanitizeCodexAlphaSearchBody`).
fn sanitize_body(body: Bytes) -> Bytes {
    let Some(mut payload) = parse_object(&body) else {
        return body;
    };
    let mut removed = false;
    for field in RESPONSES_ONLY_FIELDS {
        removed |= payload.remove(field).is_some();
    }
    if !removed {
        return body;
    }
    marshal(payload)
}

/// The payload with `model` as its model, or as it is when it names none,
/// already names that one, or isn't a JSON object
/// (`rewriteCodexAlphaSearchModel`).
fn rewrite_model(body: Bytes, model: &str) -> Bytes {
    let model = model.trim();
    if model.is_empty() {
        return body;
    }
    let Some(mut payload) = parse_object(&body) else {
        return body;
    };
    match payload.get("model") {
        None => return body,
        Some(Value::String(current)) if current == model => return body,
        Some(_) => {}
    }
    payload.insert("model".to_owned(), Value::String(model.to_owned()));
    marshal(payload)
}

/// A payload as Go's `json.Marshal` writes a `map[string]json.RawMessage`:
/// keys sorted, no spaces, and HTML characters escaped.
fn marshal(mut payload: Map<String, Value>) -> Bytes {
    payload.sort_keys();
    let text = Value::Object(payload).to_string();
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        // These only occur within strings.
        if matches!(c, '<' | '>' | '&' | '\u{2028}' | '\u{2029}') {
            let _ = write!(out, "\\u{:04x}", u32::from(c));
        } else {
            out.push(c);
        }
    }
    Bytes::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ports TestRewriteCodexAlphaSearchModel.
    #[test]
    fn rewrites_the_model() {
        let original = Bytes::from_static(
            br#"{"id":"search-1","model":"vendor/gpt-5.6-sol","commands":{"search_query":[{"q":"golang"}]}}"#,
        );
        let rewritten = rewrite_model(original, "gpt-5.6-sol");
        let payload = parse_object(&rewritten).unwrap();
        assert_eq!(payload["model"], "gpt-5.6-sol");
        assert!(
            payload.contains_key("commands"),
            "commands field was dropped"
        );
        assert_eq!(
            rewrite_model(Bytes::from_static(br#"{"query":"x"}"#), "gpt-5.6-sol"),
            r#"{"query":"x"}"#,
            "body without model should remain unchanged"
        );
    }

    // Not upstream's: the bodies a rewrite leaves alone, and how it writes
    // the one it changes.
    #[test]
    fn rewrites_only_what_it_must() {
        let same = Bytes::from_static(br#"{ "model": "gpt-5.6-sol" }"#);
        assert_eq!(rewrite_model(same.clone(), " gpt-5.6-sol "), same);
        assert_eq!(rewrite_model(same.clone(), "  "), same);
        assert_eq!(rewrite_model(Bytes::from_static(b"[1]"), "m"), "[1]");
        assert_eq!(rewrite_model(Bytes::from_static(b"{"), "m"), "{");
        assert_eq!(
            rewrite_model(
                Bytes::from_static(br#"{"z":{"b":1,"a":2.50},"model":"x","q":"<a&b>"}"#),
                "gpt"
            ),
            concat!(
                r#"{"model":"gpt","q":""#,
                "\\u003ca\\u0026b\\u003e",
                r#"","z":{"b":1,"a":2.50}}"#
            )
        );
    }

    // Not upstream's: what sanitizing removes and keeps. The handler test
    // ports TestCodexAlphaSearchSanitizesResponsesOnlyFields.
    #[test]
    fn sanitizes_responses_only_fields() {
        let body = Bytes::from_static(
            br#"{"model":"m","prompt_cache_key":"cache-123","prompt_cache_retention":"24h","id":"s"}"#,
        );
        assert_eq!(sanitize_body(body), r#"{"id":"s","model":"m"}"#);
        let untouched = Bytes::from_static(br#"{ "query": "x", "Prompt_Cache_Key": 1 }"#);
        assert_eq!(sanitize_body(untouched.clone()), untouched);
        assert_eq!(sanitize_body(Bytes::from_static(b"null")), "null");
        assert_eq!(sanitize_body(Bytes::from_static(b"not json")), "not json");
    }

    // Not upstream's: the headers every credential gets.
    #[test]
    fn copies_only_the_clients_own_headers() {
        let mut client = HeaderMap::new();
        client.insert("user-agent", HeaderValue::from_static(" my-client/1 "));
        client.insert("session_id", HeaderValue::from_static("session-123"));
        client.insert("version", HeaderValue::from_static(""));
        client.insert("originator", HeaderValue::from_static("codex_cli_rs"));
        client.insert("x-api-key", HeaderValue::from_static("nope"));
        let headers = base_headers(&client);
        let mut names: Vec<&str> = headers.keys().map(HeaderName::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["accept", "content-type", "session_id", "user-agent"]
        );
        assert_eq!(headers["user-agent"], "my-client/1");
        assert_eq!(headers["accept"], "application/json");
    }
}
