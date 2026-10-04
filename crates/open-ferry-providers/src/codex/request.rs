// Ported from CLIProxyAPI internal/runtime/executor/codex_executor_request.go,
// the body preparation of codex_executor_execute.go, codex_executor_stream.go
// and codex_executor_tokens.go, codexCreds in codex_executor_auth.go,
// codexAuthUsesAPIKey in codex_websockets_request.go, helps/codex_native.go,
// helps/payload_mutations.go, internal/util/codex.go,
// internal/util/header_helpers.go, internal/misc/header_utils.go,
// internal/thinking/suffix.go and helps/model_capabilities.go
// (ApplyRequestThinking) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The body, headers and URL of a Codex request.
//!
//! The client's payload is translated to Codex's Responses format (or, for
//! `responses/compact`, to OpenAI's), its thinking setting is applied (see
//! [`super::thinking`]), and it is adjusted as upstream does: the model
//! without its thinking suffix, `stream` forced, fields Codex refuses
//! dropped, `instructions` filled in, reasoning items and tool schemas
//! cleaned, `parallel_tool_calls` matched to the tools, and input item IDs
//! made acceptable. A credential's compatibility models and Codex clients'
//! multi-agent requests are handled as the `compat` module says.
//!
//! Deviations from upstream:
//! - No `User-Agent`, `Originator`, `Session-Id` or `X-Codex-Routing-Hint`
//!   is made up: the client's own `User-Agent` and `Originator` pass through,
//!   and otherwise the request says `User-Agent: open-ferry/<version>` and
//!   sends no `Originator`. Upstream falls back to a `codex-tui` user agent
//!   and originator, and sends a session ID and routing hint of its own.
//! - `prompt_cache_key` is only the one the client sent. Upstream makes one
//!   up from the Claude Code prompt cache, a provider session, or a hash of
//!   the client's API key, and sends it as `Session-Id` too.
//! - A `header:` attribute can't set `User-Agent`, `Originator`, a session
//!   ID or another client identity header, and one that names
//!   `$CPA-SESSION-ID` is skipped; see [`crate::custom_headers`].
//! - The config's `codex-header-defaults` user agent, models.json
//!   `override_header`, cloaking and `Connection: Keep-Alive` aren't ported.
//! - A payload that isn't a JSON object is translated as an empty object.
//! - Payload config rules aren't applied, and the original request isn't
//!   translated alongside the payload, as the payload-config module isn't
//!   ported.
//! - The image generation tool isn't added.

use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{ErrorKind, ExecError, Format, Options, Request};
use open_ferry_core::models::ModelCatalog;
use open_ferry_translate::go::trim_space;
use serde_json::{Map, Value};

use super::client::USER_AGENT;
use super::compat;
use super::ext::{self, Turn};
use super::input_ids::sanitize_input_item_ids;
use super::reasoning::sanitize_reasoning;
use super::thinking;
use super::tool_schema::normalize_tool_schemas;
use crate::custom_headers;
use crate::json::{self, delete, eq_fold, exists, get, set, str_of};
use crate::thinking::Route;

/// Codex's API, for credentials that name no `base_url`.
pub(crate) const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// The header a native Codex client sends for a Responses Lite request.
pub(crate) const RESPONSES_LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";

/// Fields Codex refuses, dropped from every request.
const DROPPED_FIELDS: [&str; 4] = [
    "previous_response_id",
    "generate",
    "prompt_cache_retention",
    "safety_identifier",
];

/// Client headers passed on when set (`misc.EnsureHeader` with no default).
const PASSED_HEADERS: [&str; 8] = [
    "version",
    "x-codex-turn-metadata",
    "x-codex-turn-state",
    "x-client-request-id",
    "x-codex-window-id",
    "thread-id",
    "session-id",
    RESPONSES_LITE_HEADER,
];

/// Which call a body is for; each prepares it a little differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A non-streaming `/responses` call, which Codex still streams.
    Execute,
    /// A streaming `/responses` call.
    Stream,
    /// A `/responses/compact` call.
    Compact,
    /// A local token count.
    CountTokens,
}

/// What a call is prepared with besides the client's request: the
/// credential it goes out with, and the executor's config and models.
#[derive(Clone, Copy, Default)]
pub(crate) struct Context<'a> {
    /// The credential.
    pub(crate) auth: Option<&'a Auth>,
    /// The proxy's config.
    pub(crate) config: Option<&'a Config>,
    /// The models the proxy serves.
    pub(crate) models: Option<&'a dyn ModelCatalog>,
}

/// A prepared request body.
#[derive(Debug)]
pub(crate) struct Body {
    /// What to send.
    pub(crate) body: Value,
    /// The translated request before the prompt cache key and item IDs were
    /// set, which response translators see as the request.
    pub(crate) translated: Value,
    /// Whether a native Codex client sent it ([`is_native`]).
    pub(crate) native: bool,
    /// What [`ext::prepare`] noted about it.
    pub(crate) turn: Turn,
}

/// The model without a thinking suffix such as `(high)`
/// (`thinking.ParseSuffix`).
pub(crate) fn base_model(model: &str) -> &str {
    match model.rfind('(') {
        Some(open) if model.ends_with(')') => model.get(..open).unwrap_or(model),
        _ => model,
    }
}

/// The format to answer in (`ResponseFormatOrSource`).
pub(crate) fn response_format(options: &Options) -> Format {
    if options.response_format.as_str().is_empty() {
        options.source_format.clone()
    } else {
        options.response_format.clone()
    }
}

/// Whether `format`, trimmed, is `want` regardless of case
/// (`sourceFormatEqual`).
pub(crate) fn format_is(format: &Format, want: &Format) -> bool {
    eq_fold(format.as_str().trim(), want.as_str())
}

/// A JSON payload as an object; anything else is an empty one.
pub(crate) fn parse_object(raw: &[u8]) -> Value {
    match serde_json::from_slice(raw) {
        Ok(value @ Value::Object(_)) => value,
        _ => Value::Object(Map::new()),
    }
}

/// The client's request, for response translators: the original request if
/// the caller kept one, else the payload.
pub(crate) fn original_request(request: &Request, options: &Options) -> Value {
    if options.original_request.is_empty() {
        parse_object(&request.payload)
    } else {
        parse_object(&options.original_request)
    }
}

/// The credential's token and base URL (`codexCreds`): the `api_key`
/// attribute, or else the OAuth access token.
pub(crate) fn credentials(auth: &Auth) -> (&str, &str) {
    let mut token = auth.attribute("api_key").unwrap_or_default();
    if token.is_empty() {
        token = auth.metadata_str("access_token").unwrap_or_default();
    }
    (token, auth.attribute("base_url").unwrap_or_default())
}

/// Whether the credential is an API key rather than a ChatGPT sign-in
/// (`codexAuthUsesAPIKey`).
pub(crate) fn uses_api_key(auth: &Auth) -> bool {
    let kind = [
        auth.attribute("auth_kind").unwrap_or_default(),
        auth.metadata_str("auth_kind").unwrap_or_default(),
    ]
    .into_iter()
    .map(normalize_auth_kind)
    .find(|kind| !kind.is_empty())
    .unwrap_or_default();
    kind == "apikey"
        || !auth
            .attribute("api_key")
            .unwrap_or_default()
            .trim()
            .is_empty()
}

/// `normalizeAuthKind`.
fn normalize_auth_kind(kind: &str) -> &'static str {
    match open_ferry_translate::go::to_lower(kind.trim()).as_str() {
        "apikey" | "api_key" | "api-key" => "apikey",
        "oauth" | "oauth2" => "oauth",
        _ => "",
    }
}

/// The URL to call: the credential's `base_url`, or `default_base`, then
/// `/responses` or `/responses/compact`.
pub(crate) fn endpoint(auth: &Auth, default_base: &str, compact: bool) -> String {
    let (_, base) = credentials(auth);
    let base = if base.is_empty() { default_base } else { base };
    let base = base.strip_suffix('/').unwrap_or(base);
    if compact {
        format!("{base}/responses/compact")
    } else {
        format!("{base}/responses")
    }
}

/// Whether the request is a Codex Responses Lite one, by its header or the
/// WebSocket metadata mirror of it (`IsCodexResponsesLiteRequest`).
pub(crate) fn is_responses_lite(body: &Value, headers: &HeaderMap) -> bool {
    if headers
        .get(RESPONSES_LITE_HEADER)
        .is_some_and(|value| eq_fold(String::from_utf8_lossy(value.as_bytes()).trim(), "true"))
    {
        return true;
    }
    match get(
        body,
        "client_metadata.ws_request_header_x_openai_internal_codex_responses_lite",
    ) {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => eq_fold(text.trim(), "true"),
        _ => false,
    }
}

/// Whether a native Codex client sent the request in Codex's own dialect,
/// so its body and output are kept as they are (`IsNativeCodexRequest`).
pub(crate) fn is_native(payload: &Value, options: &Options) -> bool {
    let native_format = |format: &Format| {
        format_is(format, &Format::CODEX) || format_is(format, &Format::OPENAI_RESPONSE)
    };
    native_format(&options.source_format)
        && native_format(&response_format(options))
        && is_responses_lite(payload, &options.headers)
}

/// `SetStringIfDifferent`.
pub(crate) fn set_string_if_different(body: &mut Value, path: &str, value: &str) {
    if !matches!(get(body, path), Some(Value::String(current)) if current == value) {
        set(body, path, Value::from(value));
    }
}

/// `SetBoolIfDifferent`.
pub(crate) fn set_bool_if_different(body: &mut Value, path: &str, value: bool) {
    if get(body, path) != Some(&Value::Bool(value)) {
        set(body, path, Value::Bool(value));
    }
}

/// Fills in an empty `instructions` when it is missing or null, unless the
/// request is native (`normalizeCodexInstructions`).
pub(crate) fn normalize_instructions(body: &mut Value, native: bool) {
    if !native && matches!(get(body, "instructions"), None | Some(Value::Null)) {
        set(body, "instructions", Value::from(""));
    }
}

/// Turns `parallel_tool_calls` off for a Responses Lite request, and drops
/// it when there are no tools (`normalizeCodexParallelToolCalls`).
pub(crate) fn normalize_parallel_tool_calls(body: &mut Value, headers: &HeaderMap) {
    if is_responses_lite(body, headers) {
        set_bool_if_different(body, "parallel_tool_calls", false);
        return;
    }
    normalize_parallel_tool_calls_for_tools(body);
}

/// `normalizeCodexParallelToolCallsForTools`.
fn normalize_parallel_tool_calls_for_tools(body: &mut Value) {
    if !exists(body, "parallel_tool_calls") {
        return;
    }
    let has_tools = matches!(get(body, "tools"), Some(Value::Array(tools)) if !tools.is_empty());
    if !has_tools {
        delete(body, "parallel_tool_calls");
    }
}

/// Translates and adjusts the payload for a call of `kind`.
pub(crate) fn prepare_body(
    kind: Kind,
    context: Context<'_>,
    request: &Request,
    options: &Options,
) -> Result<Body, ExecError> {
    let base = base_model(&request.model);
    let payload = parse_object(&request.payload);
    let native = is_native(&payload, options);
    let (to, stream) = match kind {
        Kind::Compact => (Format::OPENAI_RESPONSE, false),
        Kind::Stream => (Format::CODEX, true),
        Kind::Execute | Kind::CountTokens => (Format::CODEX, false),
    };
    let mut body = compat::translate(
        kind,
        context,
        request,
        options,
        &to,
        stream,
        payload.clone(),
    );
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

    match kind {
        Kind::Execute => {
            set_string_if_different(&mut body, "model", base);
            set_bool_if_different(&mut body, "stream", true);
            drop_fields(&mut body);
            delete(&mut body, "stream_options");
        }
        Kind::Stream => {
            drop_fields(&mut body);
            let delivery = get(&body, "stream_options.reasoning_summary_delivery").cloned();
            delete(&mut body, "stream_options");
            if let Some(delivery) = delivery {
                set(
                    &mut body,
                    "stream_options.reasoning_summary_delivery",
                    delivery,
                );
            }
            set_string_if_different(&mut body, "model", base);
        }
        Kind::Compact => {
            set_string_if_different(&mut body, "model", base);
            delete(&mut body, "stream");
        }
        Kind::CountTokens => {
            set_string_if_different(&mut body, "model", base);
            drop_fields(&mut body);
            delete(&mut body, "stream_options");
            set_bool_if_different(&mut body, "stream", false);
        }
    }
    normalize_instructions(&mut body, native);
    if kind == Kind::CountTokens {
        let translated = body.clone();
        return Ok(Body {
            body,
            translated,
            native,
            turn: Turn::default(),
        });
    }
    sanitize_reasoning(&mut body, compat::is_compat(context, request, options));
    normalize_parallel_tool_calls(&mut body, &options.headers);
    normalize_tool_schemas(&mut body);
    let turn = ext::prepare(kind, context, request, options, &mut body);

    let translated = body.clone();
    if let Some(key) = client_prompt_cache_key(&options.source_format, &payload) {
        set_string_if_different(&mut body, "prompt_cache_key", &key);
    }
    sanitize_input_item_ids(&mut body);
    Ok(Body {
        body,
        translated,
        native,
        turn,
    })
}

fn drop_fields(body: &mut Value) {
    for field in DROPPED_FIELDS {
        delete(body, field);
    }
}

/// The `prompt_cache_key` the client sent, for the formats upstream keeps it
/// from (`cacheHelper`, without the keys it makes up).
pub(crate) fn client_prompt_cache_key(source: &Format, payload: &Value) -> Option<String> {
    let key = get(payload, "prompt_cache_key")?;
    let key = if format_is(source, &Format::OPENAI_RESPONSE) {
        str_of(Some(key))
    } else if format_is(source, &Format::OPENAI) {
        str_of(Some(key)).trim().to_owned()
    } else {
        return None;
    };
    (!key.is_empty()).then_some(key)
}

/// The headers of a Codex request (`applyCodexHeadersFromSources`, without
/// the identity headers upstream makes up). `event_stream` asks for SSE.
pub(crate) fn build_headers(
    auth: &Auth,
    client: &HeaderMap,
    event_stream: bool,
) -> Result<HeaderMap, ExecError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let (token, _) = credentials(auth);
    if !token.trim().is_empty() {
        let mut value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
            ExecError::new(
                ErrorKind::Upstream,
                "codex executor: the credential's token isn't a valid header value",
            )
        })?;
        value.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, value);
    }

    if let Some(beta) = client
        .get("x-codex-beta-features")
        .filter(|value| !value.is_empty())
    {
        headers.insert(
            HeaderName::from_static("x-codex-beta-features"),
            beta.clone(),
        );
    }
    for name in PASSED_HEADERS {
        ensure_header(&mut headers, client, HeaderName::from_static(name));
    }
    if !ensure_header(&mut headers, client, header::USER_AGENT) {
        headers.insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
    }
    headers.insert(
        header::ACCEPT,
        HeaderValue::from_static(if event_stream {
            "text/event-stream"
        } else {
            "application/json"
        }),
    );
    ensure_header(&mut headers, client, HeaderName::from_static("originator"));

    if !uses_api_key(auth)
        && let Some(account) = auth.metadata_str("account_id")
    {
        match HeaderValue::from_str(account) {
            Ok(value) => {
                headers.insert(HeaderName::from_static("chatgpt-account-id"), value);
            }
            Err(_) => tracing::warn!(
                "codex: the credential's account_id isn't a valid header value; not sent"
            ),
        }
    }
    custom_headers::apply(&mut headers, &auth.attributes, client, "codex");
    Ok(headers)
}

/// Sets `name` to the client's value, trimmed, when it sent a non-empty one
/// (`misc.EnsureHeader` with no default). Returns whether it did.
pub(crate) fn ensure_header(target: &mut HeaderMap, client: &HeaderMap, name: HeaderName) -> bool {
    let Some(value) = client.get(&name) else {
        return false;
    };
    let trimmed = trim_space(value.as_bytes());
    if trimmed.is_empty() {
        return false;
    }
    match HeaderValue::from_bytes(trimmed) {
        Ok(value) => {
            target.insert(name, value);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::bool_of;
    use serde_json::json;

    fn truthy(body: &Value, path: &str) -> bool {
        bool_of(get(body, path))
    }

    fn parse(raw: &str) -> Value {
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn strips_thinking_suffix() {
        assert_eq!(base_model("gpt-5.4(high)"), "gpt-5.4");
        assert_eq!(base_model("gpt-5.4(x)(high)"), "gpt-5.4(x)");
        assert_eq!(base_model("gpt-5.4(high"), "gpt-5.4(high");
        assert_eq!(base_model("gpt-5.4"), "gpt-5.4");
    }

    // TestNormalizeCodexParallelToolCallsForTools_DropsWhenToolsMissing.
    #[test]
    fn parallel_tool_calls_dropped_without_tools() {
        let mut body = parse(r#"{"model":"gpt-5.4","parallel_tool_calls":true,"input":"hi"}"#);
        normalize_parallel_tool_calls_for_tools(&mut body);
        assert!(!exists(&body, "parallel_tool_calls"), "{body}");
    }

    // TestNormalizeCodexParallelToolCallsForTools_DropsWhenToolsEmpty.
    #[test]
    fn parallel_tool_calls_dropped_with_empty_tools() {
        let mut body =
            parse(r#"{"model":"gpt-5.4","tools":[],"parallel_tool_calls":false,"input":"hi"}"#);
        normalize_parallel_tool_calls_for_tools(&mut body);
        assert!(!exists(&body, "parallel_tool_calls"), "{body}");
        assert!(exists(&body, "tools"), "{body}");
    }

    // TestNormalizeCodexParallelToolCallsForTools_PreservesWhenToolsPresent.
    #[test]
    fn parallel_tool_calls_kept_with_tools() {
        let mut body = parse(
            r#"{"model":"gpt-5.4","tools":[{"type":"function","name":"lookup"}],"parallel_tool_calls":true,"input":"hi"}"#,
        );
        normalize_parallel_tool_calls_for_tools(&mut body);
        assert!(truthy(&body, "parallel_tool_calls"), "{body}");
    }

    // TestNormalizeCodexParallelToolCalls_ResponsesLiteMetadataForcesFalse.
    #[test]
    fn parallel_tool_calls_off_for_lite_metadata() {
        let mut body = parse(
            r#"{"model":"gpt-5.6-luna","tools":[{"type":"function","name":"lookup"}],"parallel_tool_calls":true,"client_metadata":{"ws_request_header_x_openai_internal_codex_responses_lite":"true"},"input":"hi"}"#,
        );
        normalize_parallel_tool_calls(&mut body, &HeaderMap::new());
        assert_eq!(
            get(&body, "parallel_tool_calls"),
            Some(&json!(false)),
            "{body}"
        );
    }

    // TestNormalizeCodexParallelToolCalls_ResponsesLiteHeaderForcesFalse.
    #[test]
    fn parallel_tool_calls_off_for_lite_header() {
        let mut body = parse(r#"{"model":"gpt-5.6-luna","parallel_tool_calls":true,"input":"hi"}"#);
        let mut headers = HeaderMap::new();
        headers.insert(RESPONSES_LITE_HEADER, HeaderValue::from_static("true"));
        normalize_parallel_tool_calls(&mut body, &headers);
        assert_eq!(
            get(&body, "parallel_tool_calls"),
            Some(&json!(false)),
            "{body}"
        );
    }

    #[test]
    fn recognizes_lite_requests() {
        let mut headers = HeaderMap::new();
        assert!(!is_responses_lite(&json!({}), &headers));
        assert!(is_responses_lite(
            &json!({"client_metadata": {"ws_request_header_x_openai_internal_codex_responses_lite": true}}),
            &headers
        ));
        assert!(is_responses_lite(
            &json!({"client_metadata": {"ws_request_header_x_openai_internal_codex_responses_lite": " TRUE "}}),
            &headers
        ));
        assert!(!is_responses_lite(
            &json!({"client_metadata": {"ws_request_header_x_openai_internal_codex_responses_lite": 1}}),
            &headers
        ));
        headers.insert(RESPONSES_LITE_HEADER, HeaderValue::from_static(" True "));
        assert!(is_responses_lite(&json!({}), &headers));
    }

    #[test]
    fn detects_api_keys() {
        let mut auth = Auth::default();
        assert!(!uses_api_key(&auth));
        auth.attributes.insert("api_key".into(), " sk-test ".into());
        assert!(uses_api_key(&auth));
        auth.attributes.insert("auth_kind".into(), "oauth".into());
        assert!(uses_api_key(&auth), "a set api_key still counts");
        let mut auth = Auth::default();
        auth.metadata.insert("auth_kind".into(), json!("API-Key"));
        assert!(uses_api_key(&auth));
        auth.attributes.insert("auth_kind".into(), "oauth2".into());
        assert!(!uses_api_key(&auth));
    }

    #[test]
    fn builds_endpoints() {
        let mut auth = Auth::default();
        assert_eq!(
            endpoint(&auth, DEFAULT_BASE_URL, false),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        auth.attributes
            .insert("base_url".into(), "http://127.0.0.1:9/v1/".into());
        assert_eq!(
            endpoint(&auth, DEFAULT_BASE_URL, true),
            "http://127.0.0.1:9/v1/responses/compact"
        );
    }

    #[test]
    fn prefers_api_key_to_access_token() {
        let mut auth = Auth::default();
        auth.metadata
            .insert("access_token".into(), json!("oauth-token"));
        assert_eq!(credentials(&auth).0, "oauth-token");
        auth.attributes.insert("api_key".into(), "key".into());
        assert_eq!(credentials(&auth).0, "key");
    }

    #[test]
    fn headers_identify_as_open_ferry_without_made_up_identity() {
        let mut auth = Auth::default();
        auth.metadata.insert("access_token".into(), json!("token"));
        auth.metadata.insert("account_id".into(), json!("acct_1"));
        let headers = build_headers(&auth, &HeaderMap::new(), true).unwrap();
        assert_eq!(headers.get(header::USER_AGENT).unwrap(), USER_AGENT);
        assert!(USER_AGENT.starts_with("open-ferry/"));
        assert_eq!(headers.get(header::AUTHORIZATION).unwrap(), "Bearer token");
        assert!(headers.get(header::AUTHORIZATION).unwrap().is_sensitive());
        assert_eq!(headers.get(header::ACCEPT).unwrap(), "text/event-stream");
        assert_eq!(headers.get("chatgpt-account-id").unwrap(), "acct_1");
        for absent in [
            "originator",
            "session-id",
            "session_id",
            "x-codex-routing-hint",
            "connection",
        ] {
            assert!(headers.get(absent).is_none(), "{absent}");
        }
    }

    #[test]
    fn headers_pass_client_values_through() {
        let mut auth = Auth::default();
        auth.attributes.insert("api_key".into(), "key".into());
        auth.metadata.insert("account_id".into(), json!("acct_1"));
        let mut client = HeaderMap::new();
        for (name, value) in [
            ("user-agent", " my-client/1.0 "),
            ("originator", "my_originator"),
            ("session-id", "sess-1"),
            ("version", "0.1.0"),
            ("x-client-request-id", "req-1"),
            ("x-codex-beta-features", "a,b"),
            ("thread-id", "   "),
            ("x-other", "dropped"),
        ] {
            client.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        let headers = build_headers(&auth, &client, false).unwrap();
        assert_eq!(headers.get(header::USER_AGENT).unwrap(), "my-client/1.0");
        assert_eq!(headers.get("originator").unwrap(), "my_originator");
        assert_eq!(headers.get("session-id").unwrap(), "sess-1");
        assert_eq!(headers.get("version").unwrap(), "0.1.0");
        assert_eq!(headers.get("x-client-request-id").unwrap(), "req-1");
        assert_eq!(headers.get("x-codex-beta-features").unwrap(), "a,b");
        assert_eq!(headers.get(header::ACCEPT).unwrap(), "application/json");
        assert!(headers.get("thread-id").is_none());
        assert!(headers.get("x-other").is_none());
        assert!(
            headers.get("chatgpt-account-id").is_none(),
            "API keys send no account"
        );
    }

    #[test]
    fn custom_header_attributes() {
        let mut auth = Auth::default();
        for (key, value) in [
            ("header:X-Static", " static "),
            ("header:X-From-Client", "$X-Tenant"),
            ("header:X-Missing", "$X-Not-Sent"),
            ("header:X-Session", "$CPA-SESSION-ID"),
            ("header:X-Session-Embedded", "prefix-$cpa-session-id"),
            ("header:Bad Name", "x"),
            ("header:  ", "x"),
            ("other", "x"),
        ] {
            auth.attributes.insert(key.into(), value.into());
        }
        let mut client = HeaderMap::new();
        client.insert("x-tenant", HeaderValue::from_static("tenant-1"));
        let headers = build_headers(&auth, &client, true).unwrap();
        assert_eq!(headers.get("x-static").unwrap(), "static");
        assert_eq!(headers.get("x-from-client").unwrap(), "tenant-1");
        for absent in ["x-missing", "x-session", "x-session-embedded", "other"] {
            assert!(headers.get(absent).is_none(), "{absent}");
        }
    }

    /// `header:` attributes for client identity headers, in several cases.
    const IDENTITY_ATTRIBUTES: [(&str, &str); 16] = [
        ("header:User-Agent", "codex_cli_rs/0.200.0"),
        ("header:user-agent ", "claude-cli/2.1.280"),
        ("header:USER-AGENT", "made-up/1"),
        ("header:X-App", "cli"),
        ("header:x-APP", "cli"),
        ("header:X-Stainless-Runtime", "node"),
        ("header:x-stainless-lang", "js"),
        ("header:X-STAINLESS-OS", "MacOS"),
        ("header:Originator", "codex-tui"),
        ("header:ORIGINATOR", "codex_cli_rs"),
        ("header:Session_id", "synthetic-session"),
        ("header:SESSION_ID", "synthetic-session"),
        ("header:Session-Id", "synthetic-session"),
        ("header:session-ID", "$X-Tenant"),
        ("header:X-Claude-Code-Session-Id", "synthetic-session"),
        ("header:x-claude-code-session-id", "synthetic-session"),
    ];

    /// The headers [`IDENTITY_ATTRIBUTES`] name that the client didn't send.
    const IDENTITY_HEADERS: [&str; 7] = [
        "x-app",
        "x-stainless-runtime",
        "x-stainless-lang",
        "x-stainless-os",
        "session_id",
        "session-id",
        "x-claude-code-session-id",
    ];

    // No attribute makes the request pass for another client: the client's
    // own user agent and originator stay, or this project's user agent and
    // none. Other custom headers still apply.
    #[test]
    fn custom_headers_cannot_set_the_clients_identity() {
        let mut auth = Auth::default();
        for (key, value) in IDENTITY_ATTRIBUTES {
            auth.attributes.insert(key.into(), value.into());
        }
        auth.attributes
            .insert("header:X-Team".into(), "blue".into());
        let mut client = HeaderMap::new();
        client.insert("x-tenant", HeaderValue::from_static("tenant-1"));
        let headers = build_headers(&auth, &client, true).unwrap();
        assert_eq!(headers.get(header::USER_AGENT).unwrap(), USER_AGENT);
        assert!(headers.get("originator").is_none());
        for absent in IDENTITY_HEADERS {
            assert!(headers.get(absent).is_none(), "{absent}");
        }
        assert_eq!(headers.get("x-team").unwrap(), "blue");

        client.insert(
            header::USER_AGENT,
            HeaderValue::from_static("actual-client/1"),
        );
        client.insert("originator", HeaderValue::from_static("actual_originator"));
        let headers = build_headers(&auth, &client, true).unwrap();
        assert_eq!(headers.get(header::USER_AGENT).unwrap(), "actual-client/1");
        assert_eq!(headers.get("originator").unwrap(), "actual_originator");
        for absent in IDENTITY_HEADERS {
            assert!(headers.get(absent).is_none(), "{absent}");
        }
        assert_eq!(headers.get("x-team").unwrap(), "blue");
    }

    #[test]
    fn invalid_token_error_hides_it() {
        let mut auth = Auth::default();
        auth.attributes
            .insert("api_key".into(), "secret\nvalue".into());
        let error = build_headers(&auth, &HeaderMap::new(), true).unwrap_err();
        assert!(!error.message.contains("secret"), "{}", error.message);
    }

    #[test]
    fn client_prompt_cache_keys_only() {
        let payload = json!({"prompt_cache_key": " key "});
        assert_eq!(
            client_prompt_cache_key(&Format::OPENAI_RESPONSE, &payload).as_deref(),
            Some(" key ")
        );
        assert_eq!(
            client_prompt_cache_key(&Format::OPENAI, &payload).as_deref(),
            Some("key")
        );
        assert_eq!(client_prompt_cache_key(&Format::CLAUDE, &payload), None);
        assert_eq!(client_prompt_cache_key(&Format::OPENAI, &json!({})), None);
        assert_eq!(
            client_prompt_cache_key(&Format::OPENAI, &json!({"prompt_cache_key": " "})),
            None
        );
    }

    #[test]
    fn set_if_different_keeps_equal_values() {
        let mut body = json!({"model": "a", "stream": "true"});
        set_string_if_different(&mut body, "model", "a");
        set_bool_if_different(&mut body, "stream", true);
        assert_eq!(body, json!({"model": "a", "stream": true}));
    }
}
