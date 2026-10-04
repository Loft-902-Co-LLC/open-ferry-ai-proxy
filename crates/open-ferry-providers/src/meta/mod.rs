// Ported from CLIProxyAPI internal/runtime/executor/meta_executor.go,
// meta_executor_execute.go and meta_executor_stream.go, and
// internal/runtime/executor/helps/meta_tools.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! [`MetaExecutor`], which calls Meta's API (Muse Spark models) with an API
//! key.
//!
//! Meta speaks the Responses API, as Codex does, so a call is translated to
//! Codex's format and goes to `<base>/responses`, where `<base>` is the
//! credential's `base_url` attribute (or its metadata's `base_url` or
//! `api_base_url`) or `https://api.meta.ai/v1`. The token is the credential's
//! `api_key` attribute, else its `access_token` attribute, else the same two
//! in its metadata; a call without one fails with a 401 before anything is
//! sent. Meta always streams: a call that wants one answer reads the stream
//! to its `response.completed` (or `response.incomplete`) event and
//! translates that. `responses/compact` isn't supported, and answers 501.
//!
//! What the request is made of is in the `request` module, what a failure
//! means for the credential in `error`, the stream in `stream`, and what
//! Meta's `web_search` tool takes in `tools`. A custom `apply_patch` tool
//! goes to Meta as a function, and back (see
//! [`crate::apply_patch_responses`]).
//!
//! The request carries only what the call needs: an `Authorization` bearer,
//! `Content-Type`, `Accept`, `Cache-Control`, the credential's custom
//! headers, and a user agent that is the client's own or
//! `open-ferry/<version>`. It carries nothing that names a client of Meta's
//! (see the `request` module).
//!
//! Deviations from upstream:
//! - Meta's sign-in isn't ported, nor what it brings: the executor doesn't
//!   refresh or mint tokens (the Dynamic Client Assertion exchange), turn a
//!   credential's stored sign-in into an API key (`ensureAuth`,
//!   `enrichAuth`), add the token to a plain HTTP request
//!   (`PrepareRequest`, `HttpRequest`, `PrepareRequestAuth`), or read the
//!   typed token storage. A credential holds an API key or an access token
//!   itself, and `refresh` returns it as it is.
//! - The request names no Meta client: upstream sends `X-Client-Id:
//!   tbh:tui` and a `muse-build/…` user agent, and no custom header can set
//!   either (see the crate's `custom_headers` module). See the `request`
//!   module.
//! - Requests go through `reqwest` with rustls, one shared client per proxy;
//!   upstream builds a client per request. Error bodies are read up to
//!   4 MiB, and an error body or event that quotes a secret the request
//!   sent (the token, the other headers, the URL's user info, the proxy's
//!   password) has it redacted if it is of eight bytes or more, as every
//!   client error is (see `Policy::Client` in the crate's `redact` module).
//!   So has the rest of an answer: a call that wants one answer has the
//!   stream it reads redacted whole before it is read, and a stream has
//!   each line redacted before it is read. A model can echo a secret back in
//!   its output, which upstream passes on as it is. The call's taps still
//!   see the answer as Meta sent it.
//! - Usage reporting, the served model and request logging are left to the
//!   call's taps (see the crate's `observe_send` module), and payload rules
//!   to [`crate::payload`]. The usage tap reads a Meta answer as Codex's
//!   Responses usage and records it as `MetaExecutor`.
//! - Two of upstream's 401s can't happen here and are left out: the base URL
//!   always has a default, and a call always has a credential.
//! - There is no executor hook for upstream's `SupportsApplyPatch`: the
//!   server's Codex client model list knows by its name that `meta` takes
//!   the `apply_patch` tool.
//! - The URL is read as a WHATWG URL, as for Codex (see
//!   [`crate::codex`]), and one with an ASCII control character before any
//!   `#` fails before anything is sent.

use std::borrow::Cow;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use http::{HeaderMap, Method};
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{
    ErrorKind, ExecError, Format, Options, Request, Response, StreamResponse,
};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::models::ModelCatalog;
use open_ferry_core::observe::AttemptKind;
use open_ferry_translate::registry::{Registry, ResponseContext};
use serde_json::Value;

use self::completed::translate_completed;
use self::error::wrap_upstream_error;
use self::request::{Creds, build_headers, creds, endpoint, missing_token, prepare};
use crate::codex::client::{Clients, error_chain, read_body, read_body_prefix};
use crate::codex::request::{base_model, refuse_control_characters};
use crate::codex::stream::MAX_LINE;
use crate::codex::terminal::StatusError;
use crate::codex::tokens::count_input_tokens;
use crate::codex::usage::ensure_responses_usage_details;
use crate::observe_send::{self, Attempt, BodyTap};
use crate::redact::{Policy, Secrets};

mod completed;
mod error;
mod request;
mod stream;
mod tools;

/// The `alt` of a `/responses/compact` call.
const COMPACT_ALT: &str = "responses/compact";
/// How much of an error body is read.
const MAX_ERROR_BODY: usize = 4 << 20;

/// Calls Meta (upstream's `MetaExecutor`).
pub struct MetaExecutor {
    clients: Clients,
    config: Option<Arc<Config>>,
    models: Option<Arc<dyn ModelCatalog>>,
}

impl MetaExecutor {
    /// An executor whose credentials without a `proxy_url` go through
    /// `global_proxy_url`: empty for the environment's proxy, `direct` or
    /// `none` for no proxy, or an `http` or `https` proxy URL.
    pub fn new(global_proxy_url: impl Into<String>) -> Self {
        Self {
            clients: Clients::new(global_proxy_url).for_provider("meta"),
            config: None,
            models: None,
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

    /// The URL and token of `auth`'s calls, or the 401 if it has no token
    /// (`ensureAuth`).
    fn credentials(auth: &Auth) -> Result<Creds, ExecError> {
        let creds = creds(auth);
        if creds.token.is_empty() {
            return Err(missing_token());
        }
        Ok(creds)
    }

    /// Posts `body` and returns Meta's answer, whatever its status, with the
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
        let proxy = self.clients.effective_proxy(&auth.proxy_url).to_owned();
        let secrets = observe_send::secrets(url, &headers, &proxy, auth);
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

    /// The error for Meta's failure `response`, with the `secrets` the
    /// request sent redacted.
    async fn failure(response: reqwest::Response, secrets: &Secrets) -> ExecError {
        let status = response.status().as_u16();
        let tap = BodyTap::of(&response);
        let (body, error) = read_body_prefix(response, MAX_ERROR_BODY).await;
        if let Some(error) = error {
            let error = ExecError::new(ErrorKind::Upstream, error_chain(&error));
            observe_send::attempt_error(tap.as_ref(), &error);
            return error;
        }
        tracing::debug!(status, "meta: request error");
        wrap_upstream_error(status, &body).redacted(secrets).into()
    }

    async fn execute_inner(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        refuse_compact(options)?;
        let creds = Self::credentials(auth)?;
        let mut prepared = prepare(
            self.config.as_deref(),
            self.models.as_deref(),
            request,
            options,
            true,
        )?;
        let headers = build_headers(auth, &creds.token, &options.headers)?;
        let (response, secrets) = self
            .send(
                auth,
                &endpoint(&creds.base_url),
                headers,
                &prepared.body,
                Attempt::new(
                    options,
                    AttemptKind::Execute,
                    "meta",
                    base_model(&request.model),
                    &Format::CODEX,
                    auth,
                ),
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response, &secrets).await);
        }
        let response_headers = response.headers().clone();
        let data = read_body(response, MAX_LINE)
            .await
            .map_err(|error| ExecError::new(ErrorKind::Upstream, error.to_string()))?;
        // A model can echo a secret back in its output: the answer is
        // redacted whole, before it is read, as for a client.
        let data = match secrets.bytes(&data, Policy::Client) {
            Cow::Owned(redacted) => redacted,
            Cow::Borrowed(_) => data,
        };
        let out = translate_completed(request, &mut prepared, &secrets, &data)?;
        Ok(Response {
            payload: Bytes::from(finish_payload(&prepared.response_format, out)),
            headers: response_headers,
        })
    }

    async fn execute_stream_inner(
        &self,
        auth: &Auth,
        request: Request,
        options: Options,
    ) -> Result<StreamResponse, ExecError> {
        refuse_compact(&options)?;
        let creds = Self::credentials(auth)?;
        let prepared = prepare(
            self.config.as_deref(),
            self.models.as_deref(),
            &request,
            &options,
            true,
        )?;
        let headers = build_headers(auth, &creds.token, &options.headers)?;
        let (response, secrets) = self
            .send(
                auth,
                &endpoint(&creds.base_url),
                headers,
                &prepared.body,
                Attempt::new(
                    &options,
                    AttemptKind::Stream,
                    "meta",
                    base_model(&request.model),
                    &Format::CODEX,
                    auth,
                ),
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::failure(response, &secrets).await);
        }
        let response_headers = response.headers().clone();
        let translator = Registry::global().response_stream(
            &Format::CODEX,
            &prepared.response_format,
            &ResponseContext {
                model: &request.model,
                original_request: &prepared.original,
                request: &prepared.body,
            },
        );
        let chunks = stream::translate(
            response,
            stream::Setup {
                translator,
                apply_patch: prepared.apply_patch,
                source_format: options.source_format.clone(),
                response_format: prepared.response_format,
                original: prepared.original_bytes,
                secrets,
            },
        );
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
        Self::credentials(auth)?;
        let prepared = prepare(
            self.config.as_deref(),
            self.models.as_deref(),
            request,
            options,
            false,
        )?;
        let body = prepared.body;
        let count = tokio::task::spawn_blocking(move || {
            count_input_tokens(tiktoken_rs::o200k_base_singleton(), &body)
        })
        .await
        .map_err(|_| ExecError::new(ErrorKind::Upstream, "meta executor: token counting failed"))?;
        let usage = format!(
            r#"{{"response":{{"usage":{{"input_tokens":{count},"output_tokens":0,"total_tokens":{count}}}}}}}"#
        );
        let payload = Registry::global().translate_token_count(
            &Format::CODEX,
            &prepared.response_format,
            count,
            usage.into_bytes(),
        );
        Ok(Response {
            payload: Bytes::from(payload),
            headers: HeaderMap::new(),
        })
    }
}

/// The 501 for `responses/compact`, which Meta doesn't have.
fn refuse_compact(options: &Options) -> Result<(), ExecError> {
    if options.alt == COMPACT_ALT {
        return Err(StatusError::new(501, "/responses/compact not supported").into());
    }
    Ok(())
}

/// Fills in usage details an OpenAI Responses client expects.
fn finish_payload(format: &Format, out: Vec<u8>) -> Vec<u8> {
    if *format == Format::OPENAI_RESPONSE {
        ensure_responses_usage_details(out)
    } else {
        out
    }
}

impl ProviderExecutor for MetaExecutor {
    fn id(&self) -> &str {
        "meta"
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
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        async move { self.count_tokens_inner(&auth, &request, &options).await }.boxed()
    }

    /// Meta's credentials aren't refreshed: the credential comes back as it
    /// is.
    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        async move { Ok((*auth).clone()) }.boxed()
    }
}

#[cfg(test)]
mod tests;
