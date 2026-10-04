// Ported from CLIProxyAPI internal/runtime/executor/codex_executor.go,
// codex_executor_execute.go, codex_executor_stream.go,
// codex_executor_tokens.go and codex_executor_auth.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! [`CodexExecutor`], which calls Codex with a ChatGPT sign-in or an API
//! key.
//!
//! A call goes to `<base>/responses`, where `<base>` is the credential's
//! `base_url` attribute or ChatGPT's Codex API. A client on the Responses
//! WebSocket with a credential that has `websockets` on calls over a
//! WebSocket instead (see [`super::websocket`]). Codex always streams: a
//! non-streaming call reads the stream to its `response.completed` (or
//! `response.incomplete`) event and translates that. `responses/compact`
//! goes to `<base>/responses/compact`, as OpenAI Responses, and answers
//! with JSON.
//!
//! The token is the `api_key` attribute, or else the OAuth access token,
//! which [`refresh`](CodexExecutor::refresh) renews 24 hours before it
//! expires.
//!
//! Two settings of the config's `codex` section change how calls fail:
//! `model-level-cooling` keeps a usage limit to the model rather than the
//! credential, and `stream-bootstrap-buffering` (with
//! `stream-bootstrap-timeout`) holds a stream's first lines back so that
//! an overload can fail the call over to another credential (see
//! [`super::stream`]).
//!
//! Deviations from upstream:
//! - Requests go through `reqwest` with rustls, one shared client per proxy;
//!   upstream builds a client per request, with a uTLS fingerprint for
//!   ChatGPT. The request headers are in [`super::request`].
//! - A non-streaming call reads the stream a line at a time, up to 50 MiB a
//!   line, where upstream reads the whole body first; the outcome is the
//!   same. Error bodies are read up to 4 MiB and compact bodies up to
//!   50 MiB.
//! - A dropped call or stream stops at once; upstream checks its context.
//! - An error body or terminal failure event that quotes a secret the
//!   request sent has it redacted: the credential headers after the custom
//!   ones, each cookie, the URL's credentials, the proxy's password and the
//!   credential's key or tokens (see the crate's `redact` module).
//! - Usage reporting and request logging are left to the call's taps (see
//!   the crate's `observe_send` module), and payload rules to
//!   [`crate::payload`]. The Home-service refresh isn't ported.
//! - Deferred: the image generation endpoints. See also the module docs of
//!   [`super`].
//! - One executor makes both HTTP and WebSocket calls; upstream wraps an
//!   HTTP and a WebSocket executor in a `CodexAutoExecutor`.
//! - Plain HTTP requests, which Codex Alpha Search sends, are in the
//!   `http_request` module.
//! - Refresh returns a copy of the credential with new metadata; the
//!   credential manager saves it. Upstream also updates the typed token
//!   storage, which [`Auth`] doesn't have.
//! - The URL is read as a WHATWG URL, so its `.` and `..` segments are
//!   resolved, percent-encoded ones such as `%2e%2e` included, and a `\`
//!   reads as `/`. Go sends `/a/%2e%2e/codex/responses` as written and a
//!   `\` as `%5C`. A URL with an ASCII control character before any `#`,
//!   which the WHATWG parser would drop or encode, fails before anything is
//!   sent, as Go's does, with Go's message (`net/url: invalid control
//!   character in URL`) but without the URL, which may hold a secret.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use http::{HeaderMap, Method};
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{
    ErrorKind, ExecError, Format, HttpCall, HttpReply, Options, Request, Response, StreamResponse,
};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::models::ModelCatalog;
use open_ferry_core::observe::AttemptKind;
use open_ferry_translate::go::trim_space;
use open_ferry_translate::registry::{Registry, ResponseContext};
use serde_json::Value;

use super::client::{Clients, error_chain, read_body, read_body_prefix};
use super::ext;
use super::jwt::{DEFAULT_PLAN_TYPE, parse_jwt_token};
use super::oauth::{CodexAuth, Endpoints};
use super::request::{
    Context, DEFAULT_BASE_URL, Kind, base_model, build_headers, endpoint, original_request,
    prepare_body, refuse_control_characters, response_format,
};
use super::stream::{self, Bootstrap, Clock, LineReader, MAX_LINE, StreamSetup, is_grok_client};
use super::terminal::{
    APPLY_PATCH_ERROR_MESSAGE, OutputItems, StatusError, empty_incomplete_stream_error,
    has_meaningful_output_delta, incomplete_stream_error, is_terminal_empty_incomplete,
    status_error_with_cooling, terminal_failure,
};
use super::token::{CREDENTIAL_TYPE, now_rfc3339};
use super::tokens::{count_input_tokens, tokenizer_for};
use super::usage::ensure_responses_usage_details;
use super::websocket;
use crate::json::str_at;
use crate::observe_send::{self, Attempt, BodyTap};
use crate::redact::{Policy, Secrets};

/// The `alt` of a `/responses/compact` call.
const COMPACT_ALT: &str = "responses/compact";
/// How much of an error body is read.
const MAX_ERROR_BODY: usize = 4 << 20;
/// How long before its tokens expire a credential is refreshed.
const REFRESH_LEAD: Duration = Duration::from_secs(24 * 60 * 60);
/// How many times a refresh is tried.
const REFRESH_ATTEMPTS: u32 = 3;

/// Calls Codex (upstream's `CodexExecutor`).
pub struct CodexExecutor {
    clients: Clients,
    config: Option<Arc<Config>>,
    models: Option<Arc<dyn ModelCatalog>>,
    base_url: String,
    oauth_endpoints: Endpoints,
    /// The clock of stream bootstrap buffering's time limit.
    bootstrap_clock: Clock,
    /// The Responses WebSocket sessions.
    websockets: websocket::Store,
}

impl CodexExecutor {
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
            bootstrap_clock: Arc::new(Instant::now),
            websockets: websocket::Store::new(),
        }
    }

    /// Follows `config` where upstream's executor reads its config.
    pub fn with_config(mut self, config: Arc<Config>) -> Self {
        self.config = Some(config);
        self
    }

    /// Looks up the models the proxy serves in `models`.
    pub fn with_models(mut self, models: Arc<dyn ModelCatalog>) -> Self {
        self.models = Some(models);
        self
    }

    /// Calls `base_url` for credentials without a `base_url` attribute,
    /// instead of ChatGPT's Codex API.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Refreshes tokens at `endpoints` instead of OpenAI's auth server.
    pub fn with_oauth_endpoints(mut self, endpoints: Endpoints) -> Self {
        self.oauth_endpoints = endpoints;
        self
    }

    /// Measures stream bootstrap buffering's time limit on `now`, as
    /// upstream's tests swap `codexBootstrapNow`.
    #[cfg(test)]
    pub(crate) fn with_bootstrap_clock(mut self, now: Clock) -> Self {
        self.bootstrap_clock = now;
        self
    }

    /// Closes Responses WebSocket connections after `idle` without a
    /// message, instead of five minutes.
    #[cfg(test)]
    pub(crate) fn with_websocket_idle(mut self, idle: Duration) -> Self {
        self.websockets = websocket::Store::with_idle(idle);
        self
    }

    /// The base URL for credentials without a `base_url` attribute.
    pub(super) fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The proxy setting `auth`'s calls go through (see [`Self::new`]).
    pub(super) fn proxy_for(&self, auth: &Auth) -> String {
        self.clients.effective_proxy(&auth.proxy_url).to_owned()
    }

    /// The Responses WebSocket sessions.
    pub(super) fn websockets(&self) -> &websocket::Store {
        &self.websockets
    }

    /// Whether a usage limit cools only the model, not the whole credential
    /// (`codex.model-level-cooling`, upstream's `modelLevelCooling`).
    pub(super) fn model_level_cooling(&self) -> bool {
        self.config
            .as_deref()
            .is_some_and(|config| config.codex.model_level_cooling)
    }

    /// How long a stream's first lines may be held back, when
    /// `codex.stream-bootstrap-buffering` is on.
    pub(super) fn bootstrap(&self) -> Option<Bootstrap> {
        let config = self
            .config
            .as_deref()
            .filter(|config| config.codex.stream_bootstrap_buffering)?;
        Some(Bootstrap {
            timeout: config.codex.stream_bootstrap_timeout_duration(),
            now: Arc::clone(&self.bootstrap_clock),
        })
    }

    /// What a call with `auth` is prepared with.
    pub(super) fn context<'a>(&'a self, auth: &'a Auth) -> Context<'a> {
        Context {
            auth: Some(auth),
            config: self.config.as_deref(),
            models: self.models.as_deref(),
        }
    }

    /// Posts `body` and returns Codex's answer, whatever its status, with the
    /// secrets the request sent (see [`observe_send::secrets`]).
    async fn send(
        &self,
        auth: &Auth,
        url: &str,
        headers: HeaderMap,
        body: &Value,
        attempt: Attempt<'_>,
    ) -> Result<(reqwest::Response, Secrets), ExecError> {
        refuse_control_characters(url)?;
        let body = Bytes::from(body.to_string());
        let secrets = observe_send::secrets(url, &headers, &self.proxy_for(auth), auth);
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
                ExecError::new(
                    ErrorKind::Upstream,
                    secrets.text(error_chain(&error.without_url()), Policy::Client),
                )
            })?;
        observe_send::response(tap, &mut response);
        Ok((response, secrets))
    }

    /// `Execute` for `responses/compact` (`executeCompact`).
    async fn execute_compact(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        let prepared = prepare_body(Kind::Compact, self.context(auth), request, options)?;
        let format = response_format(options);
        let headers = build_headers(auth, &options.headers, false)?;
        let url = endpoint(auth, &self.base_url, true);
        let (response, secrets) = self
            .send(
                auth,
                &url,
                headers,
                &prepared.body,
                Attempt::new(
                    options,
                    AttemptKind::Execute,
                    "codex",
                    base_model(&request.model),
                    &Format::OPENAI_RESPONSE,
                    auth,
                ),
            )
            .await?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let (body, _) = read_body_prefix(response, MAX_ERROR_BODY).await;
            tracing::debug!(status, "codex: compact request error");
            ext::on_failure(&prepared.turn, status, &body);
            let body = secrets.bytes(&body, Policy::Client);
            return Err(
                status_error_with_cooling(status, &body, self.model_level_cooling()).into(),
            );
        }
        let response_headers = response.headers().clone();
        let data = read_body(response, MAX_LINE)
            .await
            .map_err(|error| ExecError::new(ErrorKind::Upstream, error.to_string()))?;
        let data = ext::restore(&prepared.turn, &data).into_owned();
        let original = original_request(request, options);
        let context = ResponseContext {
            model: &request.model,
            original_request: &original,
            request: &prepared.translated,
        };
        let out = Registry::global()
            .translate_non_stream(&Format::OPENAI_RESPONSE, &format, &context, data)
            .filter(|out| !out.is_empty())
            .ok_or_else(|| StatusError::new(502, APPLY_PATCH_ERROR_MESSAGE))?;
        Ok(Response {
            payload: Bytes::from(finish_payload(&format, out)),
            headers: response_headers,
        })
    }

    pub(super) async fn execute_inner(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        if options.alt == COMPACT_ALT {
            return self.execute_compact(auth, request, options).await;
        }
        let prepared = prepare_body(Kind::Execute, self.context(auth), request, options)?;
        let format = response_format(options);
        let headers = build_headers(auth, &options.headers, true)?;
        let url = endpoint(auth, &self.base_url, false);
        let (response, secrets) = self
            .send(
                auth,
                &url,
                headers,
                &prepared.body,
                Attempt::new(
                    options,
                    AttemptKind::Execute,
                    "codex",
                    base_model(&request.model),
                    &Format::CODEX,
                    auth,
                ),
            )
            .await?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let (body, _) = read_body_prefix(response, MAX_ERROR_BODY).await;
            tracing::debug!(status, "codex: request error");
            ext::on_failure(&prepared.turn, status, &body);
            let body = secrets.bytes(&body, Policy::Client);
            return Err(
                status_error_with_cooling(status, &body, self.model_level_cooling()).into(),
            );
        }
        let response_headers = response.headers().clone();

        let mut reader = LineReader::new(response);
        let mut items = OutputItems::default();
        let mut saw_output_delta = false;
        while let Some(line) = reader.next_line().await {
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    tracing::debug!("codex: response read failed: {error}");
                    reader.report(&error);
                    break;
                }
            };
            let Some(rest) = line.strip_prefix(b"data:") else {
                continue;
            };
            let data = ext::restore(&prepared.turn, trim_space(rest));
            let mut event: Value = serde_json::from_slice(&data).unwrap_or(Value::Null);
            if has_meaningful_output_delta(&event) {
                saw_output_delta = true;
            }
            if let Some((error, body)) = terminal_failure(&event, self.model_level_cooling()) {
                ext::on_failure(&prepared.turn, error.status, body.as_bytes());
                return Err(error.redacted(&secrets).into());
            }
            let event_type = str_at(&event, "type");
            if event_type == "response.output_item.done" {
                items.collect(&event);
                continue;
            }
            if event_type != "response.completed" && event_type != "response.incomplete" {
                continue;
            }
            if is_terminal_empty_incomplete(&event, items.len(), saw_output_delta) {
                return Err(empty_incomplete_stream_error().into());
            }
            let completed = if items.patch(&mut event) {
                event.to_string().into_bytes()
            } else {
                data.to_vec()
            };
            ext::on_completed(&prepared.turn, &event);
            let original = original_request(request, options);
            let context = ResponseContext {
                model: &request.model,
                original_request: &original,
                request: &prepared.translated,
            };
            let out = Registry::global()
                .translate_non_stream(&Format::CODEX, &format, &context, completed)
                .filter(|out| !out.is_empty())
                .ok_or_else(|| StatusError::new(502, APPLY_PATCH_ERROR_MESSAGE))?;
            return Ok(Response {
                payload: Bytes::from(finish_payload(&format, out)),
                headers: response_headers,
            });
        }
        Err(incomplete_stream_error().into())
    }

    pub(super) async fn execute_stream_inner(
        &self,
        auth: &Auth,
        request: Request,
        options: Options,
    ) -> Result<StreamResponse, ExecError> {
        if options.alt == COMPACT_ALT {
            return Err(
                StatusError::new(400, "streaming not supported for /responses/compact").into(),
            );
        }
        let prepared = prepare_body(Kind::Stream, self.context(auth), &request, &options)?;
        let format = response_format(&options);
        let headers = build_headers(auth, &options.headers, true)?;
        let url = endpoint(auth, &self.base_url, false);
        let (response, secrets) = self
            .send(
                auth,
                &url,
                headers,
                &prepared.body,
                Attempt::new(
                    &options,
                    AttemptKind::Stream,
                    "codex",
                    base_model(&request.model),
                    &Format::CODEX,
                    auth,
                ),
            )
            .await?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let tap = BodyTap::of(&response);
            let (body, error) = read_body_prefix(response, MAX_ERROR_BODY).await;
            if let Some(error) = error {
                let error = ExecError::new(ErrorKind::Upstream, error_chain(&error));
                observe_send::attempt_error(tap.as_ref(), &error);
                return Err(error);
            }
            tracing::debug!(status, "codex: request error");
            ext::on_failure(&prepared.turn, status, &body);
            let body = secrets.bytes(&body, Policy::Client);
            return Err(
                status_error_with_cooling(status, &body, self.model_level_cooling()).into(),
            );
        }
        let response_headers = response.headers().clone();

        let original = original_request(&request, &options);
        let translator = Registry::global().response_stream(
            &Format::CODEX,
            &format,
            &ResponseContext {
                model: &request.model,
                original_request: &original,
                request: &prepared.translated,
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
            preserve_native: prepared.native,
            grok: is_grok_client(&options.headers),
            secrets,
            model_level_cooling: self.model_level_cooling(),
            turn: prepared.turn,
        };
        let chunks = match self.bootstrap() {
            Some(bootstrap) => stream::translate_buffered(response, setup, bootstrap).await?,
            None => stream::translate(response, setup),
        };
        Ok(StreamResponse {
            headers: response_headers,
            chunks,
        })
    }

    async fn count_tokens_inner(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        let prepared = prepare_body(Kind::CountTokens, self.context(auth), request, options)?;
        let model = base_model(&request.model).to_owned();
        let body = prepared.body;
        let count =
            tokio::task::spawn_blocking(move || count_input_tokens(tokenizer_for(&model), &body))
                .await
                .map_err(|_| {
                    ExecError::new(ErrorKind::Upstream, "codex executor: token counting failed")
                })?;
        let usage = format!(
            r#"{{"response":{{"usage":{{"input_tokens":{count},"output_tokens":0,"total_tokens":{count}}}}}}}"#
        );
        let payload = Registry::global().translate_token_count(
            &Format::CODEX,
            &response_format(options),
            count,
            usage.into_bytes(),
        );
        Ok(Response {
            payload: Bytes::from(payload),
            headers: HeaderMap::new(),
        })
    }

    async fn refresh_inner(&self, auth: Arc<Auth>) -> Result<Auth, ExecError> {
        tracing::debug!("codex executor: refresh called");
        let refresh_token = auth.metadata_str("refresh_token").unwrap_or_default();
        if refresh_token.is_empty() {
            return Ok((*auth).clone());
        }
        let tokens = CodexAuth::new(self.clients.get(&auth.proxy_url))
            .with_endpoints(self.oauth_endpoints.clone())
            .refresh_tokens_with_retry(refresh_token, REFRESH_ATTEMPTS)
            .await
            .map_err(|error| ExecError::new(ErrorKind::Upstream, error.message()))?;

        let mut refreshed = (*auth).clone();
        let metadata = &mut refreshed.metadata;
        metadata.insert("id_token".into(), tokens.id_token.clone().into());
        metadata.insert("access_token".into(), tokens.access_token.into());
        if !tokens.refresh_token.is_empty() {
            metadata.insert("refresh_token".into(), tokens.refresh_token.into());
        }
        if !tokens.account_id.is_empty() {
            metadata.insert("account_id".into(), tokens.account_id.into());
        }
        metadata.insert("email".into(), tokens.email.into());
        metadata.insert("expired".into(), tokens.expire.into());
        metadata.insert("type".into(), CREDENTIAL_TYPE.into());
        metadata.insert("last_refresh".into(), now_rfc3339().into());

        let mut plan_type = tokens.plan_type.trim().to_owned();
        if plan_type.is_empty()
            && !tokens.id_token.is_empty()
            && let Ok(claims) = parse_jwt_token(&tokens.id_token)
        {
            plan_type = claims.plan_type();
        }
        if plan_type.is_empty() {
            plan_type = DEFAULT_PLAN_TYPE.to_owned();
        }
        metadata.insert("plan_type".into(), plan_type.clone().into());
        refreshed.attributes.insert("plan_type".into(), plan_type);
        Ok(refreshed)
    }
}

/// Fills in usage details an OpenAI Responses client expects.
pub(super) fn finish_payload(format: &Format, out: Vec<u8>) -> Vec<u8> {
    if *format == Format::OPENAI_RESPONSE {
        ensure_responses_usage_details(out)
    } else {
        out
    }
}

impl ProviderExecutor for CodexExecutor {
    fn id(&self) -> &str {
        "codex"
    }

    fn execute(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        async move {
            if websocket::routes(&auth, &options) {
                return websocket::execute(self, &auth, &request, &options).await;
            }
            self.execute_inner(&auth, &request, &options).await
        }
        .boxed()
    }

    fn execute_stream(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
        async move {
            if websocket::routes(&auth, &options) {
                return websocket::execute_stream(self, &auth, request, options).await;
            }
            self.execute_stream_inner(&auth, request, options).await
        }
        .boxed()
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

    fn close_execution_session(&self, session_id: &str) {
        self.websockets.close(session_id);
    }

    fn http_request(
        &self,
        auth: Arc<Auth>,
        call: HttpCall,
    ) -> BoxFuture<'_, Result<HttpReply, ExecError>> {
        async move { self.http_request_inner(&auth, call).await }.boxed()
    }
}

mod http_request;

#[cfg(test)]
mod bootstrap_tests;
#[cfg(test)]
mod manager_tests;
#[cfg(test)]
mod tests;
