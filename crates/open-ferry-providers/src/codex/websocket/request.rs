// Ported from CLIProxyAPI internal/runtime/executor/codex_websockets_request.go
// (applyCodexWebsocketHeaders, ensureCodexWebsocketSessionHeader,
// codexSessionHeaderValue, applyCodexPromptCacheHeadersWithContext), the
// body preparation of codex_websockets_execute.go (Execute) and
// codex_websockets_stream.go (prepareCodexWebsocketStream), and
// codex_websockets_connection.go (buildCodexResponsesWebsocketURL,
// buildCodexWebsocketRequestBody, normalizeCodexWebsocketParallelToolCalls)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The `response.create` message, URL and handshake headers of a WebSocket
//! call.
//!
//! The body is prepared much as for HTTP (see [`crate::codex::request`]),
//! with upstream's WebSocket differences: a non-streaming call forces
//! `stream` and drops only `prompt_cache_retention` and
//! `safety_identifier`; a streaming one drops nothing; and
//! `parallel_tool_calls` is only turned off for a Responses Lite request,
//! never dropped. The message is the body with its input item IDs made
//! acceptable and `"type":"response.create"` added.
//!
//! The handshake sends the token, `OpenAI-Beta:
//! responses_websockets=2026-02-06` (or the client's own value if it names
//! `responses_websockets=`), the credential's `ChatGPT-Account-ID`, and the
//! client's own `x-codex-beta-features`, `x-codex-turn-state`,
//! `x-codex-turn-metadata`, `x-client-request-id`,
//! `x-responsesapi-include-timing-metrics`, `Version`, `User-Agent`,
//! `Originator` and session ID (as `session_id`), and for a native client
//! its Responses Lite header.
//!
//! Deviations from upstream:
//! - Nothing is made up: no `codex-tui` `User-Agent` or `Originator` (the
//!   request says `User-Agent: open-ferry/<version>` unless the client sent
//!   its own), no `session_id` for a `Mac OS` user agent, no
//!   `prompt_cache_key` from the Claude Code prompt cache or a provider
//!   session, and no `session_id` or `Conversation_id` from the prompt
//!   cache key. Only the `prompt_cache_key` an OpenAI Responses client
//!   sent goes in the body.
//! - The config's `codex-header-defaults`, cloaking (including copying a
//!   native client's thread and window headers when it's off), the routing
//!   hint and models.json `override_header` aren't ported.
//! - No `Content-Type` or `Accept` is sent, as upstream sends none.
//! - The image generation tool isn't added, and payload rules are left to
//!   [`crate::payload`], as for HTTP.
//! - The URL is read as a WHATWG URL when connecting, so its `.` and `..`
//!   segments are resolved, percent-encoded ones such as `%2e%2e` included,
//!   and a `\` reads as `/`. Gorilla sends `/a/%2e%2e/v1/responses` as
//!   written and a `\` as `%5C`. A URL with an ASCII control character
//!   before any `#` (once trimmed), which the WHATWG parser would drop or
//!   encode, fails before anything is sent, as Go's `url.Parse` does, with
//!   its message (`net/url: invalid control character in URL`) but without
//!   the URL, which may hold a secret.

use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{ErrorKind, ExecError, Format, Options, Request};
use open_ferry_translate::codex_client::multi_agent_v2;
use serde_json::Value;

use crate::codex::client::USER_AGENT;
use crate::codex::compat;
use crate::codex::ext::{self, Turn};
use crate::codex::input_ids::sanitize_input_item_ids;
use crate::codex::reasoning::sanitize_reasoning;
use crate::codex::request::{
    Context, Kind, RESPONSES_LITE_HEADER, base_model, client_prompt_cache_key, credentials,
    endpoint, ensure_header, format_is, is_native, is_responses_lite, normalize_instructions,
    parse_object, refuse_control_characters, set_bool_if_different, set_string_if_different,
    uses_api_key,
};
use crate::codex::thinking;
use crate::codex::tool_schema::normalize_tool_schemas;
use crate::custom_headers;
use crate::json::{self, delete, set};
use crate::payload;
use crate::thinking::Route;

/// The `OpenAI-Beta` value that opts into the Responses WebSocket.
pub(super) const BETA_HEADER_VALUE: &str = "responses_websockets=2026-02-06";

/// Client headers passed on when set, besides `User-Agent` and `Originator`.
const PASSED_HEADERS: [&str; 6] = [
    "x-codex-beta-features",
    "x-codex-turn-state",
    "x-codex-turn-metadata",
    "x-client-request-id",
    "x-responsesapi-include-timing-metrics",
    "version",
];

/// A prepared WebSocket call.
pub(super) struct Prepared {
    /// The body, as response translators see it.
    pub(super) body: Value,
    /// Whether a native Codex client sent it, so Codex's output is kept as
    /// it is.
    pub(super) native: bool,
    /// What [`ext::prepare`] noted about it.
    pub(super) turn: Turn,
    /// Whether the request already used the optimized collaboration
    /// namespace's name.
    pub(super) conflict: bool,
    /// Whether this request renamed the collaboration namespace.
    pub(super) optimize: bool,
    /// The `ws` or `wss` URL to connect to.
    pub(super) url: String,
    /// The handshake headers.
    pub(super) headers: HeaderMap,
    /// The `response.create` message.
    pub(super) message: String,
}

/// Prepares a WebSocket call of `kind` ([`Kind::Execute`] or
/// [`Kind::Stream`]) with `auth`, whose URL defaults to `default_base`.
pub(super) fn prepare(
    kind: Kind,
    context: Context<'_>,
    auth: &Auth,
    default_base: &str,
    request: &Request,
    options: &Options,
) -> Result<Prepared, ExecError> {
    let base = base_model(&request.model);
    let payload = parse_object(&request.payload);
    let native = is_native(&payload, options);
    let mut body = compat::translate(
        kind,
        context,
        request,
        options,
        &Format::CODEX,
        kind == Kind::Stream,
        payload.clone(),
    );
    let to = Format::CODEX;
    let route = Route {
        model: &request.model,
        from: options.source_format.as_str(),
        to: to.as_str(),
        provider: "codex",
    };
    thinking::apply_request(
        &mut body,
        route,
        &json::Body::parse(&request.payload),
        &json::Body::parse(&options.original_request),
        context.models,
    )?;
    let target = payload::Target {
        executor: "codex-websockets",
        protocol: &to,
        model: base,
        root: "",
        stream: kind == Kind::Stream,
        tracked: &[],
        translate: Some(&|payload| {
            compat::translate(
                kind,
                context,
                request,
                options,
                &to,
                kind == Kind::Stream,
                payload,
            )
        }),
    };
    payload::apply(context.config, &target, request, options, &mut body);
    set_string_if_different(&mut body, "model", base);
    if kind == Kind::Execute {
        set_bool_if_different(&mut body, "stream", true);
        delete(&mut body, "prompt_cache_retention");
        delete(&mut body, "safety_identifier");
    }
    normalize_instructions(&mut body, native);
    sanitize_reasoning(&mut body, compat::is_compat(context, request, options));
    // normalizeCodexWebsocketParallelToolCalls
    if is_responses_lite(&body, &options.headers) {
        set_bool_if_different(&mut body, "parallel_tool_calls", false);
    }
    normalize_tool_schemas(&mut body);
    let conflict = multi_agent_v2::has_namespace_conflict(&body);
    let turn = ext::prepare(kind, context, request, options, &mut body);
    let optimize = turn.multi_agent_v2_optimized();

    let url = websocket_url(&endpoint(auth, default_base, false))?;
    if format_is(&options.source_format, &Format::OPENAI_RESPONSE)
        && let Some(key) = client_prompt_cache_key(&options.source_format, &payload)
    {
        set_string_if_different(&mut body, "prompt_cache_key", &key);
    }
    let headers = build_headers(auth, &options.headers, native)?;
    let message = message(&body);
    Ok(Prepared {
        body,
        native,
        turn,
        conflict,
        optimize,
        url,
        headers,
        message,
    })
}

/// The `response.create` message for `body` (`buildCodexWebsocketRequestBody`).
pub(super) fn message(body: &Value) -> String {
    let mut message = body.clone();
    sanitize_input_item_ids(&mut message);
    set(&mut message, "type", Value::from("response.create"));
    message.to_string()
}

/// The WebSocket URL for Codex's `http_url` (`buildCodexResponsesWebsocketURL`):
/// `http` becomes `ws` and `https` becomes `wss`.
pub(super) fn websocket_url(http_url: &str) -> Result<String, ExecError> {
    let trimmed = http_url.trim();
    refuse_control_characters(trimmed)?;
    let (scheme, rest) = split_scheme(trimmed);
    let ws_scheme = match scheme.to_ascii_lowercase().as_str() {
        "http" => "ws",
        "https" => "wss",
        _ => {
            return Err(ExecError::new(
                ErrorKind::Upstream,
                format!(
                    "codex websockets executor: unsupported responses websocket URL scheme {scheme:?}"
                ),
            ));
        }
    };
    let host = rest
        .strip_prefix("//")
        .map(|authority| {
            let end = authority.find(['/', '?', '#']).unwrap_or(authority.len());
            let authority = authority.get(..end).unwrap_or_default();
            authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host)
        })
        .unwrap_or_default();
    if host.trim().is_empty() {
        return Err(ExecError::new(
            ErrorKind::Upstream,
            "codex websockets executor: responses websocket URL host is empty",
        ));
    }
    Ok(format!("{ws_scheme}:{rest}"))
}

/// A URL's scheme and the rest after its `:`, as Go's `getScheme` finds
/// them: letters first, then letters, digits, `+`, `-` or `.`. No scheme is
/// empty.
fn split_scheme(url: &str) -> (&str, &str) {
    for (index, byte) in url.bytes().enumerate() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' => {}
            b'0'..=b'9' | b'+' | b'-' | b'.' if index > 0 => {}
            b':' if index > 0 => {
                return (
                    url.get(..index).unwrap_or_default(),
                    url.get(index + 1..).unwrap_or_default(),
                );
            }
            _ => break,
        }
    }
    ("", url)
}

/// The handshake headers (`applyCodexWebsocketHeaders`, without the
/// headers upstream makes up). `native` passes a native client's
/// Responses Lite header on.
pub(super) fn build_headers(
    auth: &Auth,
    client: &HeaderMap,
    native: bool,
) -> Result<HeaderMap, ExecError> {
    let mut headers = HeaderMap::new();
    let (token, _) = credentials(auth);
    if !token.trim().is_empty() {
        let mut value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
            ExecError::new(
                ErrorKind::Upstream,
                "codex websockets executor: the credential's token isn't a valid header value",
            )
        })?;
        value.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, value);
    }
    for name in PASSED_HEADERS {
        ensure_header(&mut headers, client, HeaderName::from_static(name));
    }
    if native {
        ensure_header(
            &mut headers,
            client,
            HeaderName::from_static(RESPONSES_LITE_HEADER),
        );
    }
    if !ensure_header(&mut headers, client, header::USER_AGENT) {
        headers.insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
    }

    let beta = client_value(client, "openai-beta")
        .filter(|beta| beta.contains("responses_websockets="))
        .and_then(|beta| HeaderValue::from_str(&beta).ok())
        .unwrap_or_else(|| HeaderValue::from_static(BETA_HEADER_VALUE));
    headers.insert(HeaderName::from_static("openai-beta"), beta);

    // ensureCodexWebsocketSessionHeader, without a made-up fallback.
    if let Some(session) = ["session-id", "session_id"]
        .into_iter()
        .find_map(|name| client_value(client, name))
        .and_then(|session| HeaderValue::from_str(&session).ok())
    {
        headers.insert(HeaderName::from_static("session_id"), session);
    }

    ensure_header(&mut headers, client, HeaderName::from_static("originator"));
    if !uses_api_key(auth)
        && let Some(account) = auth
            .metadata_str("account_id")
            .map(str::trim)
            .filter(|account| !account.is_empty())
    {
        match HeaderValue::from_str(account) {
            Ok(value) => {
                headers.insert(HeaderName::from_static("chatgpt-account-id"), value);
            }
            Err(_) => tracing::warn!(
                "codex websockets: the credential's account_id isn't a valid header value; not sent"
            ),
        }
    }
    custom_headers::apply(&mut headers, &auth.attributes, client, "codex");
    Ok(headers)
}

/// The first value of the client's `name` header that isn't blank, trimmed.
fn client_value(client: &HeaderMap, name: &str) -> Option<String> {
    client
        .get_all(name)
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).trim().to_owned())
        .find(|value| !value.is_empty())
}
