// Ported from CLIProxyAPI internal/runtime/executor/openai_compat_executor.go
// (OpenAICompatExecutor: Execute, ExecuteStream, CountTokens, Refresh,
// applyPromptCacheKey, resolveCredentials, resolveCompatConfig),
// helps/payload_helpers.go (PayloadRequestedModel) and
// helps/model_capabilities.go (ApplyRequestThinking) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! [`OpenAiCompatExecutor`], which calls a provider configured under
//! `openai-compatibility`.
//!
//! A call goes to `<base_url>/chat/completions` as OpenAI Chat Completions,
//! where `<base_url>` is the credential's `base_url` attribute without a
//! trailing slash, and the credential's `api_key` attribute, if any, is
//! sent as a bearer token. A credential without a `base_url` fails with a
//! 401. A `responses/compact` call goes to `<base_url>/responses/compact` as
//! OpenAI Responses, without `stream`, and answers with JSON.
//!
//! Before the request goes out, its thinking setting is applied (see
//! [`super::thinking`]; a compact call's as for Codex), the model's entry
//! in the provider's `models` decides whether its limit is `max_tokens` or
//! `max_completion_tokens` and whether its tool results go as plain text,
//! and a provider with `support-prompt-cache-key` gets the client's
//! `prompt_cache_key`. A stream asks for usage in its last chunk. A token
//! count is made on the request with its thinking setting applied.
//!
//! Deviations from upstream:
//! - Requests go through `reqwest` with rustls, one shared client per proxy;
//!   `reqwest` adds `Accept: */*` to a request that sets no `Accept`.
//!   Error bodies are read up to 4 MiB and answers up to 50 MiB.
//! - A dropped call or stream stops at once; upstream checks its context.
//! - The body is written as `serde_json` writes it, compact.
//! - The URL is parsed as WHATWG URLs are, so `.` and `..` segments of the
//!   base URL are resolved, where Go sends them as written. A base URL with
//!   an ASCII control character fails before anything is sent, as Go's
//!   does, but the error doesn't quote the URL, which may hold a secret.
//! - An error body that quotes the credential's API key has it redacted;
//!   see [`crate::redact`].
//! - Usage reporting, request logging and the Home service (its credential
//!   options and refresh) aren't ported.
//! - See also the module docs of [`super`].

use std::sync::Arc;

use bytes::Bytes;
use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use http::{HeaderMap, HeaderValue, header};
use open_ferry_core::auth::compat::{ATTRIBUTE_COMPAT_NAME, ATTRIBUTE_PROVIDER_KEY};
use open_ferry_core::auth::{Auth, AuthSource};
use open_ferry_core::config::{Config, OpenAiCompatibility};
use open_ferry_core::exec::{
    ErrorKind, ExecError, Format, Options, Request, Response, StreamResponse,
};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::models::ModelCatalog;
use open_ferry_translate::go::trim_space;
use open_ferry_translate::registry::{Registry, ResponseContext};
use serde_json::Value;

use super::max_tokens::{normalize_max_tokens, should_use_max_completion_tokens};
use super::status::status_error;
use super::stream::{self, StreamSetup};
use super::thinking;
use super::tokens::{count_chat_tokens, tokenizer_for, usage_json};
use super::tool_results::{normalize_tool_results_text_only, should_normalize_tool_results};
use crate::codex::client::{Clients, USER_AGENT, error_chain, read_body, read_body_prefix};
use crate::codex::compat;
use crate::codex::reasoning::sanitize_reasoning;
use crate::codex::request::{
    base_model, original_request, parse_object, response_format, set_bool_if_different,
    set_string_if_different,
};
use crate::codex::stream::MAX_LINE;
use crate::codex::terminal::{APPLY_PATCH_ERROR_MESSAGE, StatusError};
use crate::codex::thinking as responses_thinking;
use crate::codex::usage::ensure_responses_usage_details;
use crate::custom_headers;
use crate::json::{Body, delete, eq_fold, str_at};
use crate::redact;
use crate::thinking::Route;

/// The `alt` of a `/responses/compact` call.
const COMPACT_ALT: &str = "responses/compact";
/// How much of an error body is read.
const MAX_ERROR_BODY: usize = 4 << 20;
/// How log lines and errors name this executor.
const NAME: &str = "openai compat executor";

/// Calls an OpenAI-compatible provider (upstream's
/// `OpenAICompatExecutor`).
pub struct OpenAiCompatExecutor {
    provider: String,
    config: Arc<Config>,
    models: Option<Arc<dyn ModelCatalog>>,
    clients: Clients,
}

/// The credential's API key, sent as a bearer token unless it is empty.
fn api_key(auth: &Auth) -> &str {
    auth.attribute("api_key").unwrap_or_default().trim()
}

/// A request ready to send.
struct Prepared {
    url: String,
    /// The provider's format.
    to: Format,
    /// The body as sent, which response translators see as the request.
    body: Value,
    headers: HeaderMap,
}

impl OpenAiCompatExecutor {
    /// An executor for credentials of `provider`, such as
    /// `openai-compatible-myprovider`, with the `openai-compatibility`
    /// entries of `config`. Credentials without a `proxy_url` go through
    /// the config's `proxy-url`.
    pub fn new(provider: impl Into<String>, config: Arc<Config>) -> Self {
        let clients = Clients::new(config.proxy_url.clone()).for_provider(NAME);
        Self {
            provider: provider.into(),
            config,
            models: None,
            clients,
        }
    }

    /// Looks up the models the proxy serves in `models`.
    pub fn with_models(mut self, models: Arc<dyn ModelCatalog>) -> Self {
        self.models = Some(models);
        self
    }

    /// The `openai-compatibility` entry `auth` belongs to
    /// (`resolveCompatConfig`): for a credential from the config file, the
    /// enabled entry at its `config_index`; else the first enabled entry
    /// named, regardless of case, by its `compat_name`, `provider_key` or
    /// provider.
    fn compat_config(&self, auth: &Auth) -> Option<&OpenAiCompatibility> {
        let entries = &self.config.openai_compatibility;
        if auth.auth_source_kind() == Some(AuthSource::Config)
            && let Some(index) = auth.attribute("config_index")
            && let Ok(index) = index.trim().parse::<i64>()
            && let Ok(index) = usize::try_from(index)
            && let Some(compat) = entries.get(index)
            && !compat.disabled
        {
            return Some(compat);
        }
        let candidates: Vec<&str> = [
            auth.attribute(ATTRIBUTE_COMPAT_NAME),
            auth.attribute(ATTRIBUTE_PROVIDER_KEY),
            Some(auth.provider.as_str()),
        ]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|candidate| !candidate.is_empty())
        .collect();
        entries
            .iter()
            .filter(|compat| !compat.disabled)
            .find(|compat| {
                candidates
                    .iter()
                    .any(|candidate| eq_fold(candidate, &compat.name))
            })
    }

    /// Translates and adjusts the client's request for the provider, as a
    /// stream or not.
    fn prepare(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
        stream: bool,
    ) -> Result<Prepared, ExecError> {
        let base_url = auth.attribute("base_url").unwrap_or_default().trim();
        let api_key = api_key(auth);
        if base_url.is_empty() {
            return Err(StatusError::new(401, "missing provider baseURL").into());
        }
        // Go's `url.Parse` refuses these; WHATWG parsing drops some of them.
        if base_url.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(ExecError::new(
                ErrorKind::Upstream,
                "net/url: invalid control character in URL",
            ));
        }
        let compact = options.alt == COMPACT_ALT;
        let (to, path, translate_stream) = if stream {
            (Format::OPENAI, "/chat/completions", true)
        } else if compact {
            (
                Format::OPENAI_RESPONSE,
                "/responses/compact",
                options.stream,
            )
        } else {
            (Format::OPENAI, "/chat/completions", options.stream)
        };
        let base = base_model(&request.model);
        let payload = parse_object(&request.payload);
        let mut prepared = payload.clone();
        compat::before_translation(Some(&self.config), options, &to, &mut prepared);
        let mut body = Registry::global().translate_request(
            &options.source_format,
            &to,
            base,
            prepared,
            translate_stream,
        );
        self.apply_thinking(&mut body, request, options, &to)?;
        compat::after_translation(options, &mut body);

        let compat = self.compat_config(auth);
        let requested = requested_model(request, options);
        if should_normalize_tool_results(compat, base, requested) {
            normalize_tool_results_text_only(&mut body);
        }
        if !compact {
            let use_max_completion_tokens =
                should_use_max_completion_tokens(compat, base, requested);
            normalize_max_tokens(&mut body, use_max_completion_tokens);
            apply_prompt_cache_key(compat, &payload, options, &mut body);
        }
        if stream {
            // Usage comes in the last chunk only when asked for.
            set_bool_if_different(&mut body, "stream_options.include_usage", true);
        } else if compact {
            delete(&mut body, "stream");
            sanitize_reasoning(&mut body, false);
        }

        let headers = build_headers(auth, api_key, &options.headers, stream)?;
        let url = format!("{}{path}", base_url.strip_suffix('/').unwrap_or(base_url));
        Ok(Prepared {
            url,
            to,
            body,
            headers,
        })
    }

    /// Applies the thinking setting of the model's suffix or of the request
    /// to `body`, translated to `to`: Chat Completions, or OpenAI Responses
    /// for a compact call.
    pub(super) fn apply_thinking(
        &self,
        body: &mut Value,
        request: &Request,
        options: &Options,
        to: &Format,
    ) -> Result<(), ExecError> {
        let route = Route {
            model: &request.model,
            from: options.source_format.as_str(),
            to: to.as_str(),
            provider: &self.provider,
        };
        let payload = Body::parse(&request.payload);
        let original = Body::parse(&options.original_request);
        let models = self.models.as_deref();
        if *to == Format::OPENAI_RESPONSE {
            responses_thinking::apply_request(body, route, &payload, &original, models)
        } else {
            thinking::apply_request(body, route, &payload, &original, models)
        }
    }

    /// Posts the prepared request and returns the provider's answer if its
    /// status is a success.
    async fn send(&self, auth: &Auth, prepared: &Prepared) -> Result<reqwest::Response, ExecError> {
        let response = self
            .clients
            .get(&auth.proxy_url)
            .post(&prepared.url)
            .headers(prepared.headers.clone())
            .body(prepared.body.to_string())
            .send()
            .await
            .map_err(|error| {
                ExecError::new(ErrorKind::Upstream, error_chain(&error.without_url()))
            })?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(response);
        }
        let headers = response.headers().clone();
        let (body, _) = read_body_prefix(response, MAX_ERROR_BODY).await;
        tracing::debug!(status, "{NAME}: request error");
        let body = redact::bytes(&body, api_key(auth));
        Err(status_error(status, &headers, &body).into())
    }

    async fn execute_inner(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        let prepared = self.prepare(auth, request, options, false)?;
        let format = response_format(options);
        let response = self.send(auth, &prepared).await?;
        let response_headers = response.headers().clone();
        let data = read_body(response, MAX_LINE)
            .await
            .map_err(|error| ExecError::new(ErrorKind::Upstream, error.to_string()))?;
        let original = original_request(request, options);
        let context = ResponseContext {
            model: &request.model,
            original_request: &original,
            request: &prepared.body,
        };
        let out = Registry::global()
            .translate_non_stream(&prepared.to, &format, &context, data)
            .filter(|out| !out.is_empty())
            .ok_or_else(|| StatusError::new(502, APPLY_PATCH_ERROR_MESSAGE))?;
        let payload = if format == Format::OPENAI_RESPONSE {
            ensure_responses_usage_details(out)
        } else {
            out
        };
        Ok(Response {
            payload: Bytes::from(payload),
            headers: response_headers,
        })
    }

    async fn execute_stream_inner(
        &self,
        auth: &Auth,
        request: Request,
        options: Options,
    ) -> Result<StreamResponse, ExecError> {
        let prepared = self.prepare(auth, &request, &options, true)?;
        let format = response_format(&options);
        let response = self.send(auth, &prepared).await?;
        let response_headers = response.headers().clone();

        let original = original_request(&request, &options);
        let translator = Registry::global().response_stream(
            &Format::OPENAI,
            &format,
            &ResponseContext {
                model: &request.model,
                original_request: &original,
                request: &prepared.body,
            },
        );
        let original_bytes = if options.original_request.is_empty() {
            request.payload.clone()
        } else {
            options.original_request.clone()
        };
        let setup = StreamSetup {
            translator,
            response_format: format,
            source_format: options.source_format.clone(),
            original: original_bytes,
            secret: api_key(auth).to_owned(),
        };
        Ok(StreamResponse {
            headers: response_headers,
            chunks: stream::translate(response, setup),
        })
    }

    async fn count_tokens_inner(
        &self,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        let model = base_model(&request.model).to_owned();
        let mut payload = parse_object(&request.payload);
        compat::before_translation(Some(&self.config), options, &Format::OPENAI, &mut payload);
        let mut body = Registry::global().translate_request(
            &options.source_format,
            &Format::OPENAI,
            &model,
            payload,
            false,
        );
        self.apply_thinking(&mut body, request, options, &Format::OPENAI)?;
        let count =
            tokio::task::spawn_blocking(move || count_chat_tokens(tokenizer_for(&model), &body))
                .await
                .map_err(|_| {
                    ExecError::new(
                        ErrorKind::Upstream,
                        format!("{NAME}: token counting failed"),
                    )
                })?;
        let payload = Registry::global().translate_token_count(
            &Format::OPENAI,
            &response_format(options),
            count,
            usage_json(count).into_bytes(),
        );
        Ok(Response {
            payload: Bytes::from(payload),
            headers: HeaderMap::new(),
        })
    }

    /// Returns the credential as it is: an API key needs no refresh. A
    /// credential with a refresh token can't be refreshed here, which is an
    /// error.
    fn refresh_inner(&self, auth: &Auth) -> Result<Auth, ExecError> {
        tracing::debug!("{NAME}: refresh called");
        let has_refresh_token = ["refresh_token", "refreshToken"].iter().any(|key| {
            auth.metadata_str(key)
                .is_some_and(|token| !token.trim().is_empty())
        });
        if has_refresh_token {
            let provider = if self.provider.is_empty() {
                auth.provider.trim()
            } else {
                self.provider.as_str()
            };
            return Err(ExecError::new(
                ErrorKind::Upstream,
                format!("{NAME} cannot refresh oauth credentials for provider {provider}"),
            ));
        }
        Ok(auth.clone())
    }
}

/// The model the client asked for (`PayloadRequestedModel`): the one the
/// handler recorded, else the request's, without the spaces around it.
fn requested_model<'a>(request: &'a Request, options: &'a Options) -> &'a str {
    let recorded = options.metadata.requested_model.trim();
    if recorded.is_empty() {
        request.model.trim()
    } else {
        recorded
    }
}

/// Passes on the client's `prompt_cache_key` to a provider that takes one
/// (`applyPromptCacheKey`): the first non-empty one in the request payload,
/// the client's original request or the translated body.
fn apply_prompt_cache_key(
    compat: Option<&OpenAiCompatibility>,
    payload: &Value,
    options: &Options,
    body: &mut Value,
) {
    if !compat.is_some_and(|compat| compat.support_prompt_cache_key) {
        return;
    }
    let original = parse_object(&options.original_request);
    let key = [payload, &original, &*body]
        .into_iter()
        .map(|source| str_at(source, "prompt_cache_key").trim().to_owned())
        .find(|key| !key.is_empty());
    if let Some(key) = key {
        set_string_if_different(body, "prompt_cache_key", &key);
    }
}

/// The request headers: a JSON body, the API key as a bearer token, the
/// client's `User-Agent` or this project's, the credential's custom
/// headers, and for a stream `Accept: text/event-stream` and
/// `Cache-Control: no-cache`.
fn build_headers(
    auth: &Auth,
    api_key: &str,
    client: &HeaderMap,
    stream: bool,
) -> Result<HeaderMap, ExecError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if !api_key.is_empty() {
        let mut value = HeaderValue::from_str(&format!("Bearer {api_key}")).map_err(|_| {
            ExecError::new(
                ErrorKind::Upstream,
                format!("{NAME}: the credential's API key isn't a valid header value"),
            )
        })?;
        value.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, value);
    }
    let user_agent = client
        .get(header::USER_AGENT)
        .map(|value| trim_space(value.as_bytes()))
        .filter(|value| !value.is_empty())
        .and_then(|value| HeaderValue::from_bytes(value).ok())
        .unwrap_or_else(|| HeaderValue::from_static(USER_AGENT));
    headers.insert(header::USER_AGENT, user_agent);
    custom_headers::apply(&mut headers, &auth.attributes, client, NAME);
    if stream {
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    Ok(headers)
}

impl ProviderExecutor for OpenAiCompatExecutor {
    fn id(&self) -> &str {
        &self.provider
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
        async move { self.execute_stream_inner(&auth, request, options).await }.boxed()
    }

    fn count_tokens(
        &self,
        _auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        async move { self.count_tokens_inner(&request, &options).await }.boxed()
    }

    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        let result = self.refresh_inner(&auth);
        async move { result }.boxed()
    }
}

#[cfg(test)]
mod tests;
