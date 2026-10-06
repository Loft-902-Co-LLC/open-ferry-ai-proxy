// Ported from CLIProxyAPI internal/runtime/executor/xai_executor_request.go
// (prepareResponsesRequestTo, xaiCreds, xaiChatBaseURL, xaiCompactBaseURL,
// applyXAIDefaultHeaders, applyXAICustomHeaders, xaiExecutionSessionID,
// normalizeXAIImageRefs, preserveXAIResponsesOutputControls) and the
// image/video routing of xai_executor_execute.go and xai_executor_media.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI
//
// Also ported from CLIProxyAPI internal/runtime/executor/helps/payload_finalizer.go
// (NewPayloadFinalizer, for xAI) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The body, headers and URL of an xAI request.
//!
//! The client's payload is translated to Codex's Responses format (or, for
//! `responses/compact`, to OpenAI's), keeps the client's output limits and
//! sampling settings, has its thinking setting applied (see
//! [`super::thinking`]), and is adjusted as upstream does: the model without
//! its thinking suffix, `stream` set as the call needs, fields xAI refuses
//! dropped, a custom `apply_patch` tool declared as a function (see
//! [`crate::apply_patch_responses`]), the tools reshaped for Grok (see
//! [`super::tools`]), reasoning xAI can't take dropped (see
//! [`super::reasoning`]), and `instructions` filled in.
//!
//! The config's payload rules apply last, to the body as it is sent
//! ([`Prepared::finalize`]): each call applies them once it has shaped the
//! body, a compact call or a token count included, and the WebSocket call to
//! its message (see [`Finalizer`]).
//!
//! Calls go to `<base>/responses` (or `/responses/compact`), where `<base>`
//! is the credential's `base_url` attribute, else its `base_url` metadata,
//! else xAI's API, `https://api.x.ai/v1`.
//!
//! Deviations from upstream:
//! - Only API keys are served. The token is the `api_key` attribute; an
//!   xAI sign-in's access token isn't read, and nothing goes to Grok's CLI
//!   chat proxy unless a credential's `base_url` names it, so `using_api`
//!   isn't read either.
//! - No session is made up. `x-grok-conv-id` and `prompt_cache_key` are the
//!   `prompt_cache_key` the client sent, trimmed, or absent. Upstream also
//!   takes a Responses WebSocket's execution session, a session it derives
//!   from the request's metadata, the Claude Code prompt cache, or a new
//!   UUID for `grok-composer-` models.
//! - Grok's CLI identity headers and its `xai-grok-workspace` user agent
//!   are never sent. `X-XAI-Token-Auth` and `x-grok-client-*` are client
//!   identity headers, which no `header:` attribute may set for any
//!   executor (see [`crate::custom_headers`]); nor may one set the chat
//!   proxy's `x-authenticateresponse` or an `x-grok-conv-id` other than the
//!   client's (see [`build_headers`]).
//! - The request says `User-Agent: open-ferry/<version>` where Go's says
//!   `Go-http-client/1.1`, and sends no `Connection: Keep-Alive`.
//! - Image and video streams and compactions (`openai-image` and
//!   `openai-video`) are refused with a 400 before anything is sent; any
//!   other image call goes to xAI's Images API, and any other video call to
//!   its video API (see the executor's `images` and `videos` modules).
//! - Image references are rewritten in place, keeping the body's key order;
//!   upstream writes the whole body again with Go's sorted keys.
//! - A payload that isn't a JSON object is translated as an empty object.
//! - Upstream translates a credential's compatibility models with its
//!   compatibility translator; no credential resolves one here (as for the
//!   other executors besides Codex; see [`crate::codex::compat`]).

use std::collections::HashSet;

use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{ErrorKind, ExecError, Format, Options, Request};
use open_ferry_translate::codex_client::{header_value, multi_agent_v2};
use open_ferry_translate::json::exact;
use open_ferry_translate::registry::Registry;
use serde_json::Value;

use super::reasoning;
use super::replay;
use super::thinking;
use super::tools::{self, ClientToolKey, NamespaceRefs};
use crate::apply_patch_responses::{self, State};
use crate::codex::client::USER_AGENT;
use crate::codex::compat;
use crate::codex::request::{
    Context, base_model, normalize_instructions, parse_object, response_format,
    set_bool_if_different, set_string_if_different,
};
use crate::codex::terminal::StatusError;
use crate::custom_headers;
use crate::json::{self, delete, get, str_of};
use crate::payload;

/// xAI's API, for credentials that name no `base_url`
/// (`xaiauth.DefaultAPIBaseURL`).
pub(crate) const DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";

/// Grok's CLI chat proxy (`xaiauth.CLIChatProxyBaseURL`), which a compact
/// call never goes to.
pub(crate) const CLI_CHAT_PROXY_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";

/// The executor's provider.
pub(crate) const PROVIDER: &str = "xai";

/// What an image or video stream or compaction is refused with.
pub(crate) const MEDIA_REFUSED: &str = "image and video generation are not supported";

/// The source formats of upstream's image and video handlers.
const MEDIA_SOURCES: [&str; 2] = ["openai-image", "openai-video"];

/// The header that names the conversation for xAI's prompt cache.
pub(crate) const CONV_ID_HEADER: &str = "x-grok-conv-id";

/// The Grok CLI chat proxy's answer to its challenge, never sent. The CLI's
/// other identity headers are dropped for every executor
/// ([`custom_headers::is_identity_header`]).
const AUTHENTICATE_RESPONSE_HEADER: &str = "x-authenticateresponse";

/// Fields xAI's Responses API refuses, dropped from every request.
const DROPPED_FIELDS: [&str; 4] = [
    "previous_response_id",
    "prompt_cache_retention",
    "safety_identifier",
    "stream_options",
];

/// A prepared request (upstream's `xaiPreparedRequest`).
pub(crate) struct Prepared {
    /// The `apply_patch` bridge for the response.
    pub(crate) apply_patch: State,
    /// The model without its thinking suffix.
    pub(crate) base_model: String,
    /// The format the client gets.
    pub(crate) response_format: Format,
    /// The format the body is in: Codex's, or OpenAI Responses for a compact
    /// call.
    pub(crate) to: Format,
    /// The client's request as it came (the original request if the caller
    /// kept one, else the payload).
    pub(crate) original_payload: Bytes,
    /// [`Prepared::original_payload`] as JSON, for response translators.
    pub(crate) original: Value,
    /// What to send.
    pub(crate) body: Value,
    /// The flattened and folded tool names, for restoring the response's
    /// calls.
    pub(crate) namespace_tools: NamespaceRefs,
    /// The function and custom tools the client declared, which the X search
    /// filter leaves alone.
    pub(crate) client_declared_tools: HashSet<ClientToolKey>,
    /// The client's `prompt_cache_key`, trimmed, or empty.
    pub(crate) session_id: String,
    /// Whether the body has Grok's X search tool, whose own calls are then
    /// dropped from the response.
    pub(crate) filter_internal_x_search: bool,
    /// What the client's `web_search` function was renamed to, or empty.
    pub(crate) web_search_alias: String,
    /// The session whose reasoning is replayed.
    pub(crate) replay: replay::Scope,
    /// Applies the config's payload rules.
    pub(crate) finalizer: Finalizer,
}

impl Prepared {
    /// Applies the config's payload rules to the body, which is then as it
    /// is sent (`prepared.finalizePayload`).
    pub(crate) fn finalize(
        &mut self,
        config: Option<&Config>,
        request: &Request,
        options: &Options,
    ) {
        self.finalizer
            .apply(config, request, options, &mut self.body);
    }
}

/// The config's payload rules for a prepared request
/// (`helps.NewPayloadFinalizer`), applied once to the body as it is sent.
pub(crate) struct Finalizer {
    /// The model without its thinking suffix.
    model: String,
    /// The format the body is in.
    to: Format,
    stream: bool,
    /// The client's request, translated as the body was, which the rules'
    /// conditions read.
    original: Value,
}

impl Finalizer {
    /// Applies the config's payload rules to `body`.
    pub(crate) fn apply(
        &self,
        config: Option<&Config>,
        request: &Request,
        options: &Options,
        body: &mut Value,
    ) {
        let target = payload::Target {
            executor: PROVIDER,
            protocol: &self.to,
            model: &self.model,
            root: "",
            stream: self.stream,
            tracked: &[],
            translate: Some(&|_| self.original.clone()),
        };
        payload::apply(config, &target, request, options, body);
    }
}

/// Whether the call is for upstream's image or video handler, whose calls
/// are refused unless the executor sends them on (see the module docs).
pub(crate) fn is_media_request(options: &Options) -> bool {
    MEDIA_SOURCES.contains(&options.source_format.as_str())
}

/// The 400 an image or video stream or compaction gets, before anything
/// is sent.
pub(crate) fn media_refused() -> ExecError {
    StatusError::new(400, MEDIA_REFUSED).into()
}

/// The credential's API key, trimmed (`xaiCreds` for an API key).
pub(crate) fn token(auth: &Auth) -> &str {
    auth.attribute("api_key").unwrap_or_default().trim()
}

/// The base URL calls go to (`xaiChatBaseURL` and `xaiCompactBaseURL` for an
/// API key): the `base_url` attribute, else the `base_url` metadata, both
/// trimmed, else xAI's API.
pub(crate) fn base_url(auth: &Auth) -> &str {
    [auth.attribute("base_url"), auth.metadata_str("base_url")]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|base| !base.is_empty())
        .unwrap_or(DEFAULT_BASE_URL)
}

/// The URL of a `/responses` call, or `/responses/compact`. A compact call
/// to Grok's CLI chat proxy goes to xAI's API instead, as upstream's
/// `xaiCompactBaseURL` sends it, since the proxy doesn't serve it.
pub(crate) fn endpoint(auth: &Auth, compact: bool) -> String {
    let mut base = base_url(auth);
    if compact && base.trim_end_matches('/') == CLI_CHAT_PROXY_BASE_URL {
        base = DEFAULT_BASE_URL;
    }
    let base = base.strip_suffix('/').unwrap_or(base);
    if compact {
        format!("{base}/responses/compact")
    } else {
        format!("{base}/responses")
    }
}

/// The session the request belongs to: the client's `prompt_cache_key`,
/// trimmed, if it sent a non-empty one. Nothing is made up when it didn't.
/// A numeric key is read as written, so `-0` stays `-0`, as in upstream.
pub(crate) fn client_session_id(payload: &[u8]) -> String {
    exact::from_slice(payload)
        .ok()
        .map(|payload| str_of(get(&payload, "prompt_cache_key")).trim().to_owned())
        .unwrap_or_default()
}

/// Translates the client's payload to `to` as upstream does for an executor
/// that isn't Codex's (`TranslateRequestWithAPIKeyModelCompatibilityForExecutor`
/// without a compatibility model).
fn translate(
    config: Option<&Config>,
    options: &Options,
    to: &Format,
    model: &str,
    mut payload: Value,
    stream: bool,
) -> Value {
    compat::before_translation(config, options, to, &mut payload);
    Registry::global().translate_request(&options.source_format, to, model, payload, stream)
}

/// Keeps the client's output limit and sampling settings, which the
/// translation to Codex's format drops (`preserveXAIResponsesOutputControls`).
/// Only a Chat Completions or OpenAI Responses `source` has them.
pub(crate) fn preserve_output_controls(body: &mut Value, source: &Value, from: &Format) {
    let present = |value: Option<&Value>| value.filter(|value| !value.is_null()).cloned();
    let max_output_tokens = if *from == Format::OPENAI {
        present(get(source, "max_completion_tokens")).or_else(|| present(get(source, "max_tokens")))
    } else if *from == Format::OPENAI_RESPONSE {
        present(get(source, "max_output_tokens"))
    } else {
        return;
    };
    if let Some(value) = max_output_tokens {
        json::set(body, "max_output_tokens", value);
    }
    for field in ["temperature", "top_p", "top_k"] {
        if let Some(value) = present(get(source, field)) {
            json::set(body, field, value);
        }
    }
}

/// Translates and adjusts the payload for a call that sends `to`
/// (`prepareResponsesRequestTo`), streaming if `stream`.
pub(crate) fn prepare(
    context: Context<'_>,
    request: &Request,
    options: &Options,
    stream: bool,
    to: Format,
) -> Result<Prepared, ExecError> {
    let base = base_model(&request.model).to_owned();
    let from = &options.source_format;
    let config = context.config;
    let original_payload = if options.original_request.is_empty() {
        request.payload.clone()
    } else {
        options.original_request.clone()
    };
    let original = parse_object(&original_payload);
    let mut original_translated = translate(config, options, &to, &base, original.clone(), stream);
    preserve_output_controls(&mut original_translated, &original, from);
    let payload = parse_object(&request.payload);
    let mut body = translate(config, options, &to, &base, payload.clone(), stream);
    preserve_output_controls(&mut body, &payload, from);

    thinking::apply_request(
        &mut body,
        &request.model,
        from.as_str(),
        &json::Body::parse(&request.payload),
        &json::Body::parse(&options.original_request),
        context.models,
    )?;
    set_string_if_different(&mut body, "model", &base);
    set_bool_if_different(&mut body, "stream", stream);
    for field in DROPPED_FIELDS {
        delete(&mut body, field);
    }
    if let Some(config) = config {
        let user_agent = header_value(
            options
                .headers
                .get_all(header::USER_AGENT)
                .iter()
                .map(HeaderValue::as_bytes),
        );
        multi_agent_v2::rewrite_input(
            &mut body,
            &user_agent,
            config.client.codex.optimize_multi_agent_v2,
            false,
        );
    }
    let mut apply_patch = State::new(from, &original, &original_translated);
    apply_patch_responses::normalize_request(&mut body, Some(&original))
        .map_err(|error| ExecError::new(ErrorKind::Upstream, error.to_string()))?;

    let inject_x_search = config.is_some_and(|config| config.xai.inject_x_search);
    let fold = tools::should_fold(&body, inject_x_search);
    let namespace_tools = tools::collect_namespace_refs(&body, fold);
    for (name, reference) in &namespace_tools {
        if reference.is_dispatcher {
            apply_patch.add_dispatcher(name, &reference.namespace);
        }
    }
    // Before namespaces are flattened, so the keys are as the client knows
    // its tools.
    let client_declared_tools = tools::collect_client_declared_tool_keys(&body);
    tools::normalize_tools(&mut body, fold);
    tools::promote_additional_tools(&mut body);
    let mut web_search_alias = String::new();
    if tools::has_client_web_search_function(&body, &namespace_tools) {
        web_search_alias = tools::resolve_client_web_search_alias(&body);
        tools::alias_client_web_search_function(&mut body, &web_search_alias, &namespace_tools);
    }
    tools::normalize_namespace_tool_choice(&mut body, fold);
    // Before the hosted tool choices are rewritten, so a model that drops
    // the tool keeps no "required" for it.
    tools::prune_orphaned_tool_choice(&mut body);
    tools::normalize_forced_hosted_tool_choice(&mut body, tools::WEB_SEARCH);
    tools::normalize_forced_hosted_tool_choice(&mut body, tools::IMAGE_GENERATION);
    tools::normalize_tool_choice_for_tools(&mut body);
    // A choice forced to a hosted tool alone would let Grok call X search
    // instead.
    if inject_x_search && !tools::requires_hosted_tool_only_any(&body) {
        tools::ensure_native_x_search(&mut body);
    }
    tools::clamp_tools(&mut body, tools::MAX_TOOLS, &namespace_tools);
    let replay = replay::apply(&mut body, request, options)?;
    tools::normalize_input_custom_tool_calls(&mut body);
    tools::normalize_input_namespace_tool_calls(&mut body, fold);
    if !web_search_alias.is_empty() {
        tools::alias_client_web_search_input(&mut body, &web_search_alias, &namespace_tools);
    }
    reasoning::normalize_input_reasoning_items(&mut body);
    reasoning::sanitize_input_encrypted_content(&mut body);

    normalize_instructions(&mut body, false);
    // Chat Completions takes `stop`; xAI's Responses API doesn't.
    delete(&mut body, "stop");
    normalize_image_refs(&mut body);

    let session_id = client_session_id(&request.payload);
    if !session_id.is_empty() {
        set_string_if_different(&mut body, "prompt_cache_key", &session_id);
    }
    let finalizer = Finalizer {
        model: base.clone(),
        to: to.clone(),
        stream,
        original: original_translated,
    };
    Ok(Prepared {
        apply_patch,
        base_model: base,
        response_format: response_format(options),
        to,
        original_payload,
        original,
        filter_internal_x_search: tools::has_native_x_search(&body),
        body,
        namespace_tools,
        client_declared_tools,
        session_id,
        web_search_alias,
        replay,
        finalizer,
    })
}

/// Rewrites OpenAI-style image references to xAI's shape anywhere in the
/// body (`normalizeXAIImageRefs`): an `image`, or an item of `images` or
/// `reference_images`, holding `image_url` (a string or `{"url": …}`) gets
/// `url` instead. A chat content part's `image_url` isn't one of them.
pub(crate) fn normalize_image_refs(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, child) in object.iter_mut() {
                match key.as_str() {
                    "image" => normalize_image_ref(child),
                    "images" | "reference_images" => {
                        if let Value::Array(refs) = child {
                            refs.iter_mut().for_each(normalize_image_ref);
                        }
                    }
                    _ => {}
                }
                normalize_image_refs(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(normalize_image_refs),
        _ => {}
    }
}

/// `normalizeXAIImageRef`.
fn normalize_image_ref(value: &mut Value) {
    let Value::Object(reference) = value else {
        return;
    };
    let original = reference.get("url").and_then(Value::as_str).unwrap_or("");
    let mut url = original.trim().to_owned();
    if url.is_empty() {
        url = match reference.get("image_url") {
            Some(Value::String(text)) => text.trim().to_owned(),
            Some(Value::Object(inner)) => inner
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned(),
            _ => String::new(),
        };
    }
    if url.is_empty() || (url == original && !reference.contains_key("image_url")) {
        return;
    }
    reference.insert("url".to_owned(), Value::String(url));
    reference.shift_remove("image_url");
}

/// The headers of an xAI request (`applyXAIHeaders` for an API key).
/// `event_stream` asks for SSE; `session_id` is the client's
/// `prompt_cache_key`, or empty.
///
/// The credential's `header:` attributes can't set a client identity header
/// (as for every executor) or the chat proxy's `x-authenticateresponse`, and
/// `x-grok-conv-id` is set to the client's session alone, whatever an
/// attribute said.
pub(crate) fn build_headers(
    auth: &Auth,
    client: &HeaderMap,
    event_stream: bool,
    session_id: &str,
) -> Result<HeaderMap, ExecError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let token = token(auth);
    if !token.is_empty() {
        let mut value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
            ExecError::new(
                ErrorKind::Upstream,
                "xai executor: the credential's token isn't a valid header value",
            )
        })?;
        value.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, value);
    }
    headers.insert(
        header::ACCEPT,
        HeaderValue::from_static(if event_stream {
            "text/event-stream"
        } else {
            "application/json"
        }),
    );
    custom_headers::apply(&mut headers, &auth.attributes, client, PROVIDER);
    strip_forbidden_headers(&mut headers);
    set_session_header(&mut headers, session_id)?;
    headers.insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
    Ok(headers)
}

/// Removes the chat proxy's `x-authenticateresponse`, which a `header:`
/// attribute may have set.
pub(crate) fn strip_forbidden_headers(headers: &mut HeaderMap) {
    if headers.remove(AUTHENTICATE_RESPONSE_HEADER).is_some() {
        tracing::warn!(
            "xai: custom header {AUTHENTICATE_RESPONSE_HEADER:?} is the Grok CLI chat proxy's; not sent"
        );
    }
}

/// Sets `x-grok-conv-id` to the client's session, or removes it when there
/// is none.
fn set_session_header(headers: &mut HeaderMap, session_id: &str) -> Result<(), ExecError> {
    let session = (!session_id.is_empty())
        .then(|| HeaderValue::from_str(session_id))
        .transpose()
        .map_err(|_| {
            ExecError::upstream(
                400,
                "xai executor: prompt_cache_key isn't a valid header value",
            )
        })?;
    if let Some(attribute) = headers.remove(CONV_ID_HEADER)
        && Some(&attribute) != session.as_ref()
    {
        tracing::warn!(
            "xai: custom header \"{CONV_ID_HEADER}\" isn't the client's prompt_cache_key; not sent"
        );
    }
    if let Some(session) = session {
        headers.insert(HeaderName::from_static(CONV_ID_HEADER), session);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
