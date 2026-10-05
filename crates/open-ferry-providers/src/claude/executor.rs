// Ported from CLIProxyAPI internal/runtime/executor/claude_executor.go,
// claude_executor_execute.go, claude_executor_stream.go,
// claude_executor_tokens.go and claude_executor_auth.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! [`ClaudeExecutor`], which calls Anthropic's Messages API with an API key
//! or a Claude sign-in.
//!
//! A call goes to `<base>/v1/messages?beta=true`, where `<base>` is the
//! credential's `base_url` attribute or Anthropic's API. A client that
//! isn't a Claude client gets Claude's stream translated even when it asked
//! for one reply: a non-streaming call then reads the whole stream, checks
//! that it holds a complete reply, and translates it. Token counts go to
//! `<base>/v1/messages/count_tokens?beta=true`.
//!
//! The key is the `api_key` attribute, or else the OAuth access token,
//! which [`refresh`](ClaudeExecutor::refresh) renews 4 hours before it
//! expires.
//!
//! Deviations from upstream:
//! - Requests go through `reqwest` with rustls, one shared client per proxy;
//!   upstream builds a client per request with a uTLS fingerprint. The
//!   request headers are in [`super::headers`] and the body changes in
//!   [`super::request`].
//! - None of upstream's Claude Code disguise is ported: no cloaking, CCH
//!   signing, CLI identity, device profiles, session IDs, MCP tool renaming,
//!   model-ID disguises or context management injection. See the module
//!   docs of [`super`].
//! - Response bodies aren't decompressed: no `Accept-Encoding` is sent, and
//!   a success with another `Content-Encoding` fails. Error bodies are read
//!   up to 4 MiB, and bodies and stream lines up to 50 MiB.
//! - Token counting for a credential that doesn't go to Anthropic's API,
//!   which upstream estimates locally, fails with a 501.
//! - A dropped call or stream stops at once; upstream checks its context.
//! - An error body, or an error event of a stream (a streamed call's or the
//!   one answering a call that isn't streamed), that quotes a secret the
//!   request sent has it redacted if it is of eight bytes or more, as
//!   every client error is (see `Policy::Client` in the crate's `redact`
//!   module): the credential headers after the custom ones, each cookie,
//!   the URL's credentials, the proxy's password and the credential's key
//!   or tokens. So has a successful answer that isn't a stream, whole,
//!   before it is checked and translated (as a token count's is), and each
//!   line of a stream; a model can echo a secret back in its output, which
//!   upstream passes on as it is. The call's taps see the answer as Claude
//!   sent it.
//! - Usage reporting and request logging are left to the call's taps,
//!   which each send tells of its attempt (see the crate's `observe_send`
//!   module), and payload rules to [`crate::payload`].
//! - The Home-service refresh, OAuth cancellation errors, API-key model
//!   compatibility and upstream model renaming aren't ported. Codex
//!   clients' requests are readied for translation as the Codex `compat`
//!   module says.
//! - Refresh returns a copy of the credential with new metadata; the
//!   credential manager saves it.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use http::{HeaderMap, Method};
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{ExecError, Format, Options, Request, Response, StreamResponse};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::models::ModelCatalog;
use open_ferry_core::observe::AttemptKind;
use open_ferry_translate::json::exact;
use open_ferry_translate::registry::{Registry, ResponseContext};
use serde_json::{Map, Value};

use super::client::{Clients, error_chain, read_body, read_body_prefix};
use super::headers::{self, Inputs};
use super::oauth::{ClaudeAuth, Endpoints, REFRESH_LEAD};
use super::ratelimit::{classify, fast_direct_error, plain_error, wrap_fast};
use super::request::{
    DEFAULT_BASE_URL, FAST_MODE_BETA, MAX_CACHE_BREAKPOINTS, TOKEN_COUNTING_BETA,
    count_cache_controls, credentials, disable_thinking_if_tool_choice_forced,
    enforce_cache_control_limit, ensure_cache_control, ensure_model_max_tokens,
    extract_and_remove_betas, is_anthropic_url, normalize_cache_control_ttl, normalize_sampling,
    sanitize_for_upstream, uses_bearer,
};
use super::stream::{self, MAX_LINE, StreamSetup, apply_patch_error};
use super::thinking;
use super::token::{CREDENTIAL_TYPE, now_rfc3339};
use super::usage::ensure_responses_usage_details;
use crate::codex::compat;
use crate::json::{self, Body};
use crate::observe_send::{self, Attempt, BodyTap};
use crate::payload;
use crate::redact::{Policy, Secrets};

/// The `alt` of a `/responses/compact` call.
const COMPACT_ALT: &str = "responses/compact";
/// How much of an error body is read.
const MAX_ERROR_BODY: usize = 4 << 20;
/// How many times a refresh is tried.
const REFRESH_ATTEMPTS: u32 = 3;

/// Calls Claude (upstream's `ClaudeExecutor`).
pub struct ClaudeExecutor {
    clients: Clients,
    config: Option<Arc<Config>>,
    models: Option<Arc<dyn ModelCatalog>>,
    base_url: String,
    oauth_endpoints: Endpoints,
    model_level_cooling: bool,
}

impl ClaudeExecutor {
    /// An executor whose credentials without a `proxy_url` go through
    /// `global_proxy_url`: empty for the environment's proxy, `direct` or
    /// `none` for no proxy, or an `http` or `https` proxy URL.
    pub fn new(global_proxy_url: impl Into<String>) -> Self {
        Self {
            clients: Clients::new(global_proxy_url),
            config: None,
            models: None,
            base_url: DEFAULT_BASE_URL.to_owned(),
            oauth_endpoints: Endpoints::default(),
            model_level_cooling: false,
        }
    }

    /// Readies Codex clients' requests as `config` says before translating
    /// them.
    pub fn with_config(mut self, config: Arc<Config>) -> Self {
        self.config = Some(config);
        self
    }

    /// Looks up models in `models`, to give a request without `max_tokens`
    /// the model's limit.
    pub fn with_models(mut self, models: Arc<dyn ModelCatalog>) -> Self {
        self.models = Some(models);
        self
    }

    /// Calls `base_url` for credentials without a `base_url` attribute,
    /// instead of Anthropic's API. It is treated as Anthropic's API.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Refreshes tokens at `endpoints` instead of Anthropic's auth server.
    pub fn with_oauth_endpoints(mut self, endpoints: Endpoints) -> Self {
        self.oauth_endpoints = endpoints;
        self
    }

    /// Keeps even an account-wide rate limit with the model rather than the
    /// credential (`claude.model-level-cooling`).
    pub fn with_model_level_cooling(mut self, enabled: bool) -> Self {
        self.model_level_cooling = enabled;
        self
    }

    /// Where a credential's requests go.
    fn target(&self, auth: &Auth) -> Target {
        let (key, base_url) = credentials(auth);
        let bearer = uses_bearer(auth, &key);
        if base_url.is_empty() {
            return Target {
                key,
                bearer,
                base_url: self.base_url.clone(),
                first_party: true,
            };
        }
        let first_party = is_anthropic_url(&base_url);
        Target {
            key,
            bearer,
            base_url,
            first_party,
        }
    }

    /// The Messages body for Claude, from the client's request.
    fn prepare_messages(
        &self,
        request: &Request,
        options: &Options,
        base_model: &str,
        upstream_stream: bool,
        set_stream: bool,
    ) -> Result<Prepared, ExecError> {
        let config = self.config.as_deref();
        let mut body = translate_request(config, request, options, base_model, upstream_stream)?;
        // Upstream tracks paths only for its cloaking, which isn't ported.
        let target = payload::Target {
            executor: "claude",
            protocol: &Format::CLAUDE,
            model: base_model,
            root: "",
            stream: upstream_stream,
            tracked: &[],
            translate: None,
        };
        payload::apply(config, &target, request, options, &mut body);
        ensure_model_max_tokens(&mut body, base_model, self.models.as_deref());
        disable_thinking_if_tool_choice_forced(&mut body);
        normalize_sampling(&mut body);
        if count_cache_controls(&body) == 0 {
            ensure_cache_control(&mut body);
        }
        enforce_cache_control_limit(&mut body, MAX_CACHE_BREAKPOINTS);
        normalize_cache_control_ttl(&mut body);
        if set_stream && body.get("stream") != Some(&Value::Bool(upstream_stream)) {
            json::set(&mut body, "stream", Value::Bool(upstream_stream));
        }
        let extra_betas = extract_and_remove_betas(&mut body);
        let translation = body.clone();
        sanitize_for_upstream(&mut body, base_model);
        Ok(Prepared {
            upstream: body,
            translation,
            extra_betas,
        })
    }

    /// Posts `body`, telling the call's taps; with the answer come the
    /// secrets the request sent (see [`observe_send::secrets`]).
    async fn send(
        &self,
        auth: &Auth,
        url: &str,
        headers: HeaderMap,
        body: &Value,
        attempt: Attempt<'_>,
    ) -> Result<(reqwest::Response, Secrets), ExecError> {
        let body = Bytes::from(body.to_string());
        let secrets = observe_send::secrets(
            url,
            &headers,
            self.clients.effective_proxy(&auth.proxy_url),
            auth,
        );
        let tap = attempt.observation.map(|observation| {
            observe_send::announce(
                observation,
                &attempt.request(&Method::POST, url, &headers, &body, &secrets),
            )
        });
        let mut response = self
            .clients
            .get(&auth.proxy_url)
            .post(url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|error| {
                plain_error(secrets.text(error_chain(&error.without_url()), Policy::Client))
            })?;
        observe_send::response(tap, &mut response);
        Ok((response, secrets))
    }

    /// Claude's answer if its status is a success and its body is plain;
    /// else the error, classified as upstream does, with the `secrets` the
    /// request sent redacted from it.
    async fn check(
        &self,
        response: reqwest::Response,
        fast: bool,
        secrets: &Secrets,
    ) -> Result<reqwest::Response, ExecError> {
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let headers = response.headers().clone();
            let body = match read_body_prefix(response, MAX_ERROR_BODY).await {
                Ok(body) => body,
                Err(error) => {
                    let message = format!("failed to read error response body: {error}");
                    tracing::warn!("claude: {message}");
                    message.into_bytes()
                }
            };
            tracing::debug!(status, "claude: request error");
            let body = secrets.bytes(&body, Policy::Client);
            return Err(if fast {
                fast_direct_error(status, &headers, &body)
            } else {
                classify(status, &headers, &body, self.model_level_cooling)
            });
        }
        if let Some(encoding) = compressed(response.headers()) {
            let error = plain_error(format!(
                "claude executor: unsupported response content encoding {encoding:?}"
            ));
            observe_send::attempt_error(BodyTap::of(&response).as_ref(), &error);
            return Err(wrap_fast(fast, status, error));
        }
        Ok(response)
    }

    async fn execute_inner(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        if options.alt == COMPACT_ALT {
            return Err(compact_error());
        }
        let base_model = thinking::parse_suffix(&request.model).0;
        let target = self.target(auth);
        let url = format!("{}/v1/messages?beta=true", target.base_url);
        let format = response_format(options);
        // A client that needs translating gets it from Claude's stream.
        let upstream_stream = format != Format::CLAUDE;
        let prepared =
            self.prepare_messages(request, options, base_model, upstream_stream, true)?;
        let headers = build_headers(&target, &prepared, auth, options, upstream_stream, false);
        let fast = is_fast(&target, &headers, &prepared.upstream);

        let (response, secrets) = self
            .send(
                auth,
                &url,
                headers,
                &prepared.upstream,
                Attempt::new(
                    options,
                    AttemptKind::Execute,
                    "claude",
                    base_model,
                    &Format::CLAUDE,
                    auth,
                ),
            )
            .await
            .map_err(|error| wrap_fast(fast, 0, error))?;
        let response = self.check(response, fast, &secrets).await?;
        let status = response.status().as_u16();
        let response_headers = response.headers().clone();
        let tap = BodyTap::of(&response);
        let data = read_body(response, MAX_LINE)
            .await
            .map_err(|error| wrap_fast(fast, status, plain_error(error.to_string())))?;
        let data = redact_answer(&secrets, data);
        if upstream_stream {
            stream::validate(&data).map_err(|mut error| {
                error.message = secrets.text(std::mem::take(&mut error.message), Policy::Client);
                observe_send::attempt_error(tap.as_ref(), &error);
                wrap_fast(fast, status, error)
            })?;
        }

        let original = original_request(request, options);
        let context = ResponseContext {
            model: &request.model,
            original_request: &original,
            request: &prepared.translation,
        };
        let mut out = Registry::global()
            .translate_non_stream(&Format::CLAUDE, &format, &context, data)
            .filter(|out| !out.is_empty())
            .ok_or_else(apply_patch_error)?;
        if format == Format::OPENAI_RESPONSE {
            out = ensure_responses_usage_details(out);
        }
        Ok(Response {
            payload: Bytes::from(out),
            headers: response_headers,
        })
    }

    async fn execute_stream_inner(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<StreamResponse, ExecError> {
        if options.alt == COMPACT_ALT {
            return Err(compact_error());
        }
        let base_model = thinking::parse_suffix(&request.model).0;
        let target = self.target(auth);
        let url = format!("{}/v1/messages?beta=true", target.base_url);
        let format = response_format(options);
        let prepared = self.prepare_messages(request, options, base_model, true, false)?;
        let headers = build_headers(&target, &prepared, auth, options, true, false);
        let fast = is_fast(&target, &headers, &prepared.upstream);

        let (response, secrets) = self
            .send(
                auth,
                &url,
                headers,
                &prepared.upstream,
                Attempt::new(
                    options,
                    AttemptKind::Stream,
                    "claude",
                    base_model,
                    &Format::CLAUDE,
                    auth,
                ),
            )
            .await
            .map_err(|error| wrap_fast(fast, 0, error))?;
        let response = self.check(response, fast, &secrets).await?;
        let status = response.status().as_u16();
        let response_headers = response.headers().clone();

        let translator = (format != Format::CLAUDE).then(|| {
            let original = original_request(request, options);
            let context = ResponseContext {
                model: &request.model,
                original_request: &original,
                request: &prepared.translation,
            };
            Registry::global().response_stream(&Format::CLAUDE, &format, &context)
        });
        let chunks = stream::forward(
            response,
            StreamSetup {
                translator,
                responses: format == Format::OPENAI_RESPONSE,
                fast,
                status,
                secrets,
            },
        );
        Ok(StreamResponse {
            headers: response_headers,
            chunks,
        })
    }

    /// `CountTokens` through Anthropic's own count (`countTokensUpstream`).
    async fn count_tokens_inner(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        let target = self.target(auth);
        if target.key.trim().is_empty() || !target.first_party {
            return Err(ExecError::upstream(
                501,
                "claude executor: counting tokens for this credential needs a local estimate, which isn't supported",
            ));
        }
        let base_model = thinking::parse_suffix(&request.model).0;
        let url = format!("{}/v1/messages/count_tokens?beta=true", target.base_url);
        let format = response_format(options);
        // A streaming translation keeps tool calls, except from Claude.
        let translate_stream = options.source_format != Format::CLAUDE;
        let config = self.config.as_deref();
        let mut body = translate_request(config, request, options, base_model, translate_stream)?;
        enforce_cache_control_limit(&mut body, MAX_CACHE_BREAKPOINTS);
        normalize_cache_control_ttl(&mut body);
        let mut extra_betas = extract_and_remove_betas(&mut body);
        extra_betas.push(TOKEN_COUNTING_BETA.to_owned());
        sanitize_for_upstream(&mut body, base_model);
        // Anthropic's count_tokens rejects these.
        for field in ["metadata", "context_management", "diagnostics"] {
            json::delete(&mut body, field);
        }
        let headers = headers::build(&Inputs {
            key: &target.key,
            bearer: target.bearer,
            first_party: target.first_party,
            stream: false,
            count_tokens: true,
            extra_betas: &extra_betas,
            body: &body,
            client: &options.headers,
            attributes: &auth.attributes,
        });

        let (response, secrets) = self
            .send(
                auth,
                &url,
                headers,
                &body,
                Attempt::new(
                    options,
                    AttemptKind::CountTokens,
                    "claude",
                    base_model,
                    &Format::CLAUDE,
                    auth,
                ),
            )
            .await?;
        let response = self.check(response, false, &secrets).await?;
        let response_headers = response.headers().clone();
        let data = read_body(response, MAX_LINE)
            .await
            .map_err(|error| plain_error(error.to_string()))?;
        let data = redact_answer(&secrets, data);
        let count = serde_json::from_slice::<Value>(&data)
            .map_or(0, |value| json::int_at(&value, "input_tokens"));
        let out = Registry::global().translate_token_count(&Format::CLAUDE, &format, count, data);
        Ok(Response {
            payload: Bytes::from(out),
            headers: response_headers,
        })
    }

    async fn refresh_inner(&self, auth: Arc<Auth>) -> Result<Auth, ExecError> {
        tracing::debug!("claude executor: refresh called");
        let refresh_token = auth
            .metadata_str("refresh_token")
            .filter(|token| !token.is_empty())
            .or_else(|| auth.metadata_str("refreshToken"))
            .unwrap_or_default();
        if refresh_token.is_empty() {
            return Ok((*auth).clone());
        }
        let tokens = ClaudeAuth::new(self.clients.get(&auth.proxy_url))
            .with_endpoints(self.oauth_endpoints.clone())
            .refresh_tokens_with_retry(refresh_token, REFRESH_ATTEMPTS)
            .await
            .map_err(|error| plain_error(error.message()))?;

        let mut refreshed = (*auth).clone();
        let metadata = &mut refreshed.metadata;
        metadata.insert("access_token".into(), tokens.access_token.into());
        // The account fields may be missing when only the token rotated; the
        // ones known before stay.
        for (key, value) in [
            ("refresh_token", tokens.refresh_token),
            ("email", tokens.email),
            ("account_uuid", tokens.account_uuid),
            ("organization_uuid", tokens.organization_uuid),
            ("organization_name", tokens.organization_name),
        ] {
            if !value.trim().is_empty() {
                metadata.insert(key.into(), value.into());
            }
        }
        metadata.insert("expired".into(), tokens.expire.into());
        metadata.insert("type".into(), CREDENTIAL_TYPE.into());
        metadata.insert("last_refresh".into(), now_rfc3339().into());
        Ok(refreshed)
    }
}

/// Where a credential's requests go.
struct Target {
    /// The API key or OAuth access token.
    key: String,
    /// Whether the key goes in a Bearer header.
    bearer: bool,
    base_url: String,
    /// Whether `base_url` is Anthropic's API.
    first_party: bool,
}

/// A Messages body ready to send.
struct Prepared {
    /// What goes to Claude.
    upstream: Value,
    /// What the response translators see, before the signature clean-up.
    translation: Value,
    /// The body's `betas`, for the header.
    extra_betas: Vec<String>,
}

/// The client's request in Claude's format, for `base_model`, with its
/// thinking setting applied.
fn translate_request(
    config: Option<&Config>,
    request: &Request,
    options: &Options,
    base_model: &str,
    stream: bool,
) -> Result<Value, ExecError> {
    let from = &options.source_format;
    let mut payload = parse_object(&request.payload);
    compat::before_translation(config, options, &Format::CLAUDE, &mut payload);
    let mut body =
        Registry::global().translate_request(from, &Format::CLAUDE, base_model, payload, stream);
    if json::str_at(&body, "model") != base_model {
        json::set(&mut body, "model", Value::String(base_model.to_owned()));
    }
    thinking::apply_request(
        &mut body,
        &request.model,
        from.as_str(),
        &Body::parse(&request.payload),
        &Body::parse(&options.original_request),
    )?;
    Ok(body)
}

fn build_headers(
    target: &Target,
    prepared: &Prepared,
    auth: &Auth,
    options: &Options,
    stream: bool,
    count_tokens: bool,
) -> HeaderMap {
    headers::build(&Inputs {
        key: &target.key,
        bearer: target.bearer,
        first_party: target.first_party,
        stream,
        count_tokens,
        extra_betas: &prepared.extra_betas,
        body: &prepared.upstream,
        client: &options.headers,
        attributes: &auth.attributes,
    })
}

/// Whether a request to Anthropic's API asks for fast mode, by its betas or
/// its `speed` (`claudeRequestIsFast`).
fn is_fast(target: &Target, headers: &HeaderMap, body: &Value) -> bool {
    if !target.first_party {
        return false;
    }
    let beta = headers.get_all("anthropic-beta").iter().any(|value| {
        String::from_utf8_lossy(value.as_bytes())
            .split(',')
            .any(|beta| beta.trim() == FAST_MODE_BETA)
    });
    beta || body
        .get("speed")
        .and_then(Value::as_str)
        .is_some_and(|speed| json::eq_fold(speed.trim(), "fast"))
}

/// A `Content-Encoding` other than `identity`, which the executor can't read.
fn compressed(headers: &HeaderMap) -> Option<String> {
    let encoding = headers
        .get_all(http::header::CONTENT_ENCODING)
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    (!encoding.is_empty() && !encoding.eq_ignore_ascii_case("identity")).then_some(encoding)
}

/// `data`, a successful answer's body, with the secrets the request sent
/// (`secrets`, from the send) redacted as for a client, so a model that
/// echoes one back doesn't hand it on.
fn redact_answer(secrets: &Secrets, data: Vec<u8>) -> Vec<u8> {
    match secrets.bytes(&data, Policy::Client) {
        Cow::Owned(redacted) => redacted,
        Cow::Borrowed(_) => data,
    }
}

fn compact_error() -> ExecError {
    ExecError::upstream(501, "/responses/compact not supported")
}

/// The format to answer in (`ResponseFormatOrSource`).
fn response_format(options: &Options) -> Format {
    if options.response_format.as_str().is_empty() {
        options.source_format.clone()
    } else {
        options.response_format.clone()
    }
}

/// A JSON object from `raw`, or an empty one, each number as the client
/// wrote it (see [`exact`]).
fn parse_object(raw: &[u8]) -> Value {
    match exact::from_slice(raw) {
        Ok(value @ Value::Object(_)) => value,
        _ => Value::Object(Map::new()),
    }
}

/// The client's request, for response translators: the original request if
/// the caller kept one, else the payload (`ApplyPatchOriginalRequest`).
fn original_request(request: &Request, options: &Options) -> Value {
    if options.original_request.is_empty() {
        parse_object(&request.payload)
    } else {
        parse_object(&options.original_request)
    }
}

impl ProviderExecutor for ClaudeExecutor {
    fn id(&self) -> &str {
        "claude"
    }

    fn execute(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        async move { self.execute_inner(&auth, &request, &options).await }.boxed()
    }

    fn execute_stream(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
        async move { self.execute_stream_inner(&auth, &request, &options).await }.boxed()
    }

    fn count_tokens(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        async move { self.count_tokens_inner(&auth, &request, &options).await }.boxed()
    }

    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        self.refresh_inner(auth).boxed()
    }

    fn refresh_lead(&self) -> Option<Duration> {
        Some(REFRESH_LEAD)
    }
}

#[cfg(test)]
mod tests;
