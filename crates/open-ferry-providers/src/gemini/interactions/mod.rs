// Ported from CLIProxyAPI internal/runtime/executor/gemini_executor.go
// (geminiInteractionsAPIRevision, NewGeminiInteractionsExecutor,
// Identifier, RequestToFormat, the Interactions branches of Execute and
// ExecuteStream, executeInteractions, executeInteractionsStream,
// shouldExecuteNativeInteractions, nativeInteractionsSourceFormat,
// isNativeInteractionsAuth) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! [`InteractionsExecutor`]: the `gemini-interactions` provider, which
//! calls Gemini's Interactions API with the credential's API key.
//!
//! A request from an Interactions, OpenAI Chat Completions, OpenAI
//! Responses, Claude or Gemini client, with a `gemini-interactions`
//! credential, goes to `{base_url}/v1beta/interactions`: the client's
//! request is translated to Interactions (an Interactions client's is
//! copied), its `model` set to the model without its suffix, its thinking
//! setting applied (`thinking`), the payload rules applied, and the IDs
//! the API rejects removed from its input (`request`). A stream asks for
//! one with `stream: true`, and its frames are translated to the client's
//! format, or passed on as they came to an Interactions client (`stream`).
//! A stream to an OpenAI Responses client whose request declares the custom
//! `apply_patch` tool fails with the patch error if it ends before it
//! finishes, even if the API sent nothing at all
//! (`InitializeApplyPatchStream`; see `initialize_stream` in the crate's
//! `apply_patch_responses` module).
//! Any other request, and every token count, is the Gemini executor's,
//! which calls `generateContent` (see [`super::GeminiExecutor`]).
//!
//! A request carries the API key as `x-goog-api-key`, a JSON
//! `Content-Type`, the client's `User-Agent` or this project's, the
//! credential's `header:` attributes, and an `Api-Revision`: the
//! credential's, else the client's, else `2026-05-20`. Nothing else
//! identifies the caller: no `x-goog-api-client` is sent, even when the
//! credential has the attribute.
//!
//! Deviations from upstream, besides those in [`super`]:
//! - A call handed to the Gemini executor is recorded as the `gemini`
//!   provider's, and looks its model up as `gemini` registered it; upstream
//!   names the executor `gemini-interactions` for both. Natively, models are
//!   looked up as `gemini` registered them, as upstream does.
//! - `RequestToFormat` is [`InteractionsExecutor::request_to_format`]: the
//!   executor trait has no such method, so nothing outside asks it.
//! - An error answer is read up to 4 MiB, with the secrets of eight bytes or
//!   more the request sent redacted, as in every client error (see
//!   `Policy::Client`); upstream reads it whole, as it is. A successful
//!   answer that isn't a stream has them redacted too, whole, before it is
//!   translated, as each line of a stream has; upstream passes it on as it
//!   is.
//! - Refresh returns the credential as it is; the Home service isn't
//!   ported.

mod request;
mod stream;
mod thinking;

use std::sync::Arc;

use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use http::HeaderMap;
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{ExecError, Format, Options, Request, Response, StreamResponse};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::models::ModelCatalog;
use open_ferry_core::observe::AttemptKind;
use open_ferry_translate::go::equal_fold;
use open_ferry_translate::registry::{Registry, ResponseContext};
use serde_json::Value;

use self::request::{
    api_key, apply_api_revision, interactions_url, sanitize_input_ids, translate_answer,
    translate_pair,
};
use self::stream::StreamSetup;
use super::{Credential, GeminiExecutor, build_headers, post, read_answer, reject_compact};
use crate::apply_patch_responses;
use crate::codex::client::Clients;
use crate::codex::request::{
    base_model, original_request, response_format, set_bool_if_different, set_string_if_different,
};
use crate::json::{self, Body};
use crate::observe_send::Attempt;
use crate::payload;
use crate::redact::Secrets;

/// How log lines and errors name this executor.
const NAME: &str = "gemini interactions executor";
/// The executor's identifier, and the provider of the credentials it calls
/// Interactions with.
const PROVIDER: &str = "gemini-interactions";

/// Calls Gemini's Interactions API with API keys, and the Gemini API for
/// what Interactions doesn't serve (upstream's `GeminiExecutor` made by
/// `NewGeminiInteractionsExecutor`).
pub struct InteractionsExecutor {
    gemini: GeminiExecutor,
    clients: Clients,
    config: Option<Arc<Config>>,
    models: Option<Arc<dyn ModelCatalog>>,
}

impl InteractionsExecutor {
    /// An executor whose credentials without a `proxy_url` go through
    /// `global_proxy_url`: empty for the environment's proxy, `direct` or
    /// `none` for no proxy, or an `http`, `https` or `socks5` proxy URL.
    pub fn new(global_proxy_url: impl Into<String>) -> Self {
        let global_proxy_url = global_proxy_url.into();
        Self {
            gemini: GeminiExecutor::new(global_proxy_url.clone()),
            clients: Clients::new(global_proxy_url).for_provider(NAME),
            config: None,
            models: None,
        }
    }

    /// Looks up models in `models`, for their thinking support, before the
    /// built-in catalog.
    pub fn with_models(mut self, models: Arc<dyn ModelCatalog>) -> Self {
        self.gemini = self.gemini.with_models(Arc::clone(&models));
        self.models = Some(models);
        self
    }

    /// Applies `config`'s payload rules, and readies Codex clients'
    /// requests as it says before translating them.
    pub fn with_config(mut self, config: Arc<Config>) -> Self {
        self.gemini = self.gemini.with_config(Arc::clone(&config));
        self.config = Some(config);
        self
    }

    /// `RequestToFormat`: the format a request from a client of `options`'
    /// format goes upstream in: Interactions for a client format the API is
    /// called natively for, else Gemini's.
    pub fn request_to_format(&self, options: &Options) -> Format {
        if native_source_format(&options.source_format) {
            Format::INTERACTIONS
        } else {
            Format::GEMINI
        }
    }

    fn models(&self) -> Option<&dyn ModelCatalog> {
        self.models.as_deref()
    }

    /// The body of an Interactions call, and the client's request
    /// translated as the payload rules' defaults check it.
    fn prepare(
        &self,
        request: &Request,
        options: &Options,
        stream: bool,
    ) -> Result<Value, ExecError> {
        let config = self.config.as_deref();
        let base = base_model(&request.model);
        let (original, mut body) = translate_pair(config, request, options, base, stream);
        if json::exists(&body, "model") && !base.is_empty() {
            set_string_if_different(&mut body, "model", base);
        }
        thinking::apply_request(
            &mut body,
            &request.model,
            options.source_format.as_str(),
            &Body::parse(&request.payload),
            &Body::parse(&options.original_request),
            self.models(),
        )?;
        let translate = |_: Value| original.clone();
        let target = payload::Target {
            executor: PROVIDER,
            protocol: &Format::INTERACTIONS,
            model: base,
            root: "",
            stream,
            tracked: &[],
            translate: Some(&translate),
        };
        payload::apply(config, &target, request, options, &mut body);
        sanitize_input_ids(&mut body);
        if stream {
            set_bool_if_different(&mut body, "stream", true);
        }
        Ok(body)
    }

    /// The request headers (`applyGeminiHeaders` and the revision).
    fn headers(auth: &Auth, options: &Options) -> Result<HeaderMap, ExecError> {
        let mut headers = build_headers(auth, options, &Credential::ApiKey(api_key(auth)), NAME)?;
        apply_api_revision(&mut headers, &options.headers);
        Ok(headers)
    }

    /// Posts `body` to the Interactions endpoint for a call of `kind`.
    async fn send(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
        body: &Value,
        kind: AttemptKind,
    ) -> Result<(reqwest::Response, Secrets), ExecError> {
        let headers = Self::headers(auth, options)?;
        post(
            &self.clients,
            &interactions_url(auth),
            headers,
            body,
            NAME,
            Attempt::new(
                options,
                kind,
                PROVIDER,
                base_model(&request.model),
                &Format::INTERACTIONS,
                auth,
            ),
        )
        .await
    }

    /// `executeInteractions`.
    async fn execute_inner(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        reject_compact(options)?;
        let body = self.prepare(request, options, false)?;
        let (response, secrets) = self
            .send(auth, request, options, &body, AttemptKind::Execute)
            .await?;
        let (headers, data) = read_answer(response, &secrets).await?;
        translate_answer(request, options, &body, headers, data)
    }

    /// `executeInteractionsStream`.
    async fn execute_stream_inner(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<StreamResponse, ExecError> {
        reject_compact(options)?;
        let body = self.prepare(request, options, true)?;
        let (response, secrets) = self
            .send(auth, request, options, &body, AttemptKind::Stream)
            .await?;
        Ok(translate_stream(response, request, options, &body, secrets))
    }
}

/// Translates the Interactions stream in `response`, to the request `sent`
/// with `secrets`, into the client's format.
fn translate_stream(
    response: reqwest::Response,
    request: &Request,
    options: &Options,
    sent: &Value,
    secrets: Secrets,
) -> StreamResponse {
    let headers = response.headers().clone();
    let format = response_format(options);
    let original = original_request(request, options);
    let mut translator = Registry::global().response_stream(
        &Format::INTERACTIONS,
        &format,
        &ResponseContext {
            model: &request.model,
            original_request: &original,
            request: sent,
        },
    );
    apply_patch_responses::initialize_stream(&mut translator, &format, &original);
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
        secrets,
        name: NAME,
    };
    StreamResponse {
        headers,
        chunks: stream::translate(response, setup),
    }
}

/// `shouldExecuteNativeInteractions`: whether a request with `options` and
/// `auth` goes to the Interactions API.
fn should_execute_native(auth: &Auth, options: &Options) -> bool {
    native_source_format(&options.source_format) && is_native_auth(auth)
}

/// `nativeInteractionsSourceFormat`: whether a client of `format` is served
/// through the Interactions API.
fn native_source_format(format: &Format) -> bool {
    [
        Format::INTERACTIONS,
        Format::OPENAI,
        Format::OPENAI_RESPONSE,
        Format::CLAUDE,
        Format::GEMINI,
    ]
    .contains(format)
}

/// `isNativeInteractionsAuth`: whether `auth` is a `gemini-interactions`
/// credential.
fn is_native_auth(auth: &Auth) -> bool {
    equal_fold(auth.provider.trim(), PROVIDER)
}

impl ProviderExecutor for InteractionsExecutor {
    fn id(&self) -> &str {
        PROVIDER
    }

    fn execute(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        if !should_execute_native(&auth, &options) {
            return self.gemini.execute(auth, request, options);
        }
        async move { self.execute_inner(&auth, &request, &options).await }.boxed()
    }

    fn execute_stream(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
        if !should_execute_native(&auth, &options) {
            return self.gemini.execute_stream(auth, request, options);
        }
        async move { self.execute_stream_inner(&auth, &request, &options).await }.boxed()
    }

    /// Counts tokens with the Gemini API's `countTokens`, as upstream's
    /// shared executor does.
    fn count_tokens(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        self.gemini.count_tokens(auth, request, options)
    }

    /// Returns the credential as it is: an API key needs no refresh.
    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        let auth = Auth::clone(&auth);
        async move { Ok(auth) }.boxed()
    }
}

#[cfg(test)]
mod tests;
