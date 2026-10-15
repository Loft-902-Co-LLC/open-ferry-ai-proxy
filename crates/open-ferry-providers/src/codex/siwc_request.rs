// Ported from CLIProxyAPI internal/runtime/executor/codex_executor_request.go
// and codex_executor_auth.go (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Shared SIWC request rules for HTTP and WebSocket callers. Identity is
//! enforced after custom headers; session and request IDs come only from
//! the caller, independently. No device identity is sent in API requests.

use http::header::{self, HeaderMap, HeaderValue};
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{ErrorKind, ExecError};
use serde_json::Value;

use super::siwc;
use crate::json::{delete, get, set};

pub(crate) const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Fail closed, without falling back to the outer credential or API key.
pub(crate) fn ensure_credentials(auth: &Auth) -> Result<(), ExecError> {
    // As upstream, SIWC ID-token signatures and nonces aren't verified,
    // and the SIWC account isn't matched to the outer account.
    if auth.use_siwc() && auth.access_token().is_empty() {
        return Err(ExecError::new(
            ErrorKind::Upstream,
            "codex executor: use_siwc is enabled but siwc access_token is missing",
        )
        .with_status(401));
    }
    Ok(())
}

/// Whether the original payload supplies non-empty instructions.
pub(crate) fn explicit_instructions(payload: &Value) -> bool {
    get(payload, "instructions")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
}

/// SIWC stripping, before payload finalization. `compact` removes stream
/// and store; ordinary Responses calls always ask for SSE without storage.
pub(crate) fn apply_body(body: &mut Value, explicit: bool, compact: bool) {
    if compact {
        delete(body, "stream");
        delete(body, "store");
    } else {
        set(body, "stream", Value::Bool(true));
        set(body, "store", Value::Bool(false));
    }
    for field in [
        "prompt_cache_retention",
        "prompt_cache_options",
        "max_output_tokens",
        "max_completion_tokens",
        "temperature",
    ] {
        delete(body, field);
    }
    if !explicit
        && get(body, "instructions").is_some_and(|value| {
            value.is_null() || value.as_str().is_some_and(|text| text.trim().is_empty())
        })
    {
        delete(body, "instructions");
    }
}

/// Enforces L1/L4/L5 after all header sources. Caller IDs are independent;
/// custom headers cannot replace either one or the bearer token.
pub(crate) fn enforce_headers(
    headers: &mut HeaderMap,
    auth: &Auth,
    caller: &HeaderMap,
) -> Result<(), ExecError> {
    ensure_credentials(auth)?;
    let forbidden: Vec<_> = headers
        .keys()
        .filter(|name| {
            let name = name.as_str();
            name.starts_with("x-codex-")
                || matches!(
                    name,
                    "originator"
                        | "chatgpt-account-id"
                        | "conversation_id"
                        | "conversation-id"
                        | "version"
                        | "thread-id"
                        | "thread_id"
                        | "x-openai-internal-codex-responses-lite"
                        | "x-openai-client-request-time"
                        | "session-id"
                        | "session_id"
                        | "x-client-request-id"
                )
        })
        .cloned()
        .collect();
    for name in forbidden {
        headers.remove(name);
    }
    let ua = HeaderValue::from_str(&siwc::user_agent())
        .expect("open-ferry SIWC user agent is a header value");
    headers.insert(header::USER_AGENT, ua);
    let mut token =
        HeaderValue::from_str(&format!("Bearer {}", auth.access_token())).map_err(|_| {
            ExecError::new(
                ErrorKind::Upstream,
                "codex executor: the credential's token isn't a valid header value",
            )
        })?;
    token.set_sensitive(true);
    headers.insert(header::AUTHORIZATION, token);
    if !super::request::ensure_header(headers, caller, "session_id".parse().unwrap())
        && let Some(session) = caller.get("session-id")
    {
        let mut source = HeaderMap::new();
        source.insert("session_id", session.clone());
        super::request::ensure_header(headers, &source, "session_id".parse().unwrap());
    }
    super::request::ensure_header(headers, caller, "x-client-request-id".parse().unwrap());
    Ok(())
}
