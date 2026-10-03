// Ported from CLIProxyAPI internal/runtime/executor/gemini_executor.go
// (GeminiExecutor: Execute, ExecuteStream, CountTokens, Refresh,
// geminiAPIKey, resolveGeminiBaseURL, capGeminiMaxOutputTokens)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! [`GeminiExecutor`]: calls the Gemini API with the credential's API key.
//!
//! A request goes to `{base_url}/v1beta/models/{model}:{action}`, where the
//! base URL is the credential's `base_url` attribute or Google's, and the
//! key is sent as `x-goog-api-key`. Before it goes, a `maxOutputTokens`
//! above the model's output limit is lowered to the limit.
//!
//! Deviations from upstream, besides those in [`super`]:
//! - A call with no `countTokens` action in its metadata is the only kind:
//!   `execute` always generates content, so its request always gets an
//!   empty user turn at both ends if it needs one.
//! - Refresh returns the credential as it is; the Home service isn't
//!   ported.

use std::sync::Arc;

use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{ExecError, Options, Request, Response, StreamResponse};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::models::ModelCatalog;
use open_ferry_translate::signature::sanitize_gemini_request_thought_signatures;
use serde_json::Value;

use super::stream::Lines;
use super::{
    Credential, build_headers, post, prepare_count_body, read_answer, reject_compact, thinking,
    translate_answer, translate_count, translate_request, translate_stream, turns,
};
use crate::codex::client::Clients;
use crate::codex::request::{base_model, set_string_if_different};
use crate::json;

/// How log lines and errors name this executor.
const NAME: &str = "gemini executor";
/// The provider whose models the executor looks up.
const PROVIDER: &str = "gemini";
/// Google's Gemini API.
const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";
/// The API version in request paths.
const API_VERSION: &str = "v1beta";

/// Calls the Gemini API with API keys (upstream's `GeminiExecutor`).
pub struct GeminiExecutor {
    clients: Clients,
    models: Option<Arc<dyn ModelCatalog>>,
}

impl GeminiExecutor {
    /// An executor whose credentials without a `proxy_url` go through
    /// `global_proxy_url`: empty for the environment's proxy, `direct` or
    /// `none` for no proxy, or an `http`, `https` or `socks5` proxy URL.
    pub fn new(global_proxy_url: impl Into<String>) -> Self {
        Self {
            clients: Clients::new(global_proxy_url).for_provider(NAME),
            models: None,
        }
    }

    /// Looks up models in `models`, for their thinking support and output
    /// limit, before the built-in catalog.
    pub fn with_models(mut self, models: Arc<dyn ModelCatalog>) -> Self {
        self.models = Some(models);
        self
    }

    fn models(&self) -> Option<&dyn ModelCatalog> {
        self.models.as_deref()
    }

    /// The body of a `generateContent` or `streamGenerateContent` call.
    fn prepare(
        &self,
        request: &Request,
        options: &Options,
        stream: bool,
    ) -> Result<Value, ExecError> {
        let base = base_model(&request.model);
        let mut body = translate_request(request, options, stream, self.models(), PROVIDER)?;
        set_string_if_different(&mut body, "model", base);
        cap_max_output_tokens(&mut body, base, self.models());
        sanitize_gemini_request_thought_signatures(&mut body, "contents");
        turns::ensure_boundary_user(&mut body, "contents");
        json::delete(&mut body, "session_id");
        Ok(body)
    }

    async fn execute_inner(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        reject_compact(options)?;
        let body = self.prepare(request, options, false)?;
        let mut url = model_url(auth, base_model(&request.model), "generateContent");
        if !options.alt.is_empty() {
            url.push_str("?$alt=");
            url.push_str(&options.alt);
        }
        let headers = build_headers(auth, options, &Credential::ApiKey(api_key(auth)), NAME)?;
        let client = self.clients.get(&auth.proxy_url);
        let response = post(&client, &url, headers, &body, NAME).await?;
        let (headers, data) = read_answer(response).await?;
        translate_answer(request, options, &body, headers, data)
    }

    async fn execute_stream_inner(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<StreamResponse, ExecError> {
        reject_compact(options)?;
        let body = self.prepare(request, options, true)?;
        let mut url = model_url(auth, base_model(&request.model), "streamGenerateContent");
        if options.alt.is_empty() {
            url.push_str("?alt=sse");
        } else {
            url.push_str("?$alt=");
            url.push_str(&options.alt);
        }
        let headers = build_headers(auth, options, &Credential::ApiKey(api_key(auth)), NAME)?;
        let client = self.clients.get(&auth.proxy_url);
        let response = post(&client, &url, headers, &body, NAME).await?;
        Ok(translate_stream(
            response,
            request,
            options,
            &body,
            Lines::Gemini,
            NAME,
        ))
    }

    async fn count_tokens_inner(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        let base = base_model(&request.model);
        let mut body = translate_request(request, options, false, self.models(), PROVIDER)?;
        prepare_count_body(&mut body, base);
        let url = model_url(auth, base, "countTokens");
        let headers = build_headers(auth, options, &Credential::ApiKey(api_key(auth)), NAME)?;
        let client = self.clients.get(&auth.proxy_url);
        let response = post(&client, &url, headers, &body, NAME).await?;
        let (headers, data) = read_answer(response).await?;
        Ok(translate_count(options, headers, data))
    }
}

/// `geminiAPIKey`: the credential's `api_key` attribute, as it is.
fn api_key(auth: &Auth) -> &str {
    auth.attribute("api_key").unwrap_or_default()
}

/// `resolveGeminiBaseURL`: the credential's `base_url` attribute without
/// surrounding space or a trailing `/`, else Google's API.
fn base_url(auth: &Auth) -> &str {
    let custom = auth.attribute("base_url").unwrap_or_default().trim();
    if custom.is_empty() {
        return DEFAULT_BASE_URL;
    }
    match custom.trim_end_matches('/') {
        "" => DEFAULT_BASE_URL,
        base => base,
    }
}

/// The URL of `action` on `model`.
fn model_url(auth: &Auth, model: &str, action: &str) -> String {
    format!("{}/{API_VERSION}/models/{model}:{action}", base_url(auth))
}

/// `capGeminiMaxOutputTokens`: lowers a numeric
/// `generationConfig.maxOutputTokens` above `model`'s output limit, or else
/// its completion limit, to that limit. Models are looked up as the
/// `gemini` provider registered them in `models`, else in the built-in
/// catalog; an unknown model, or one without limits, keeps what was asked.
fn cap_max_output_tokens(body: &mut Value, model: &str, models: Option<&dyn ModelCatalog>) {
    let path = "generationConfig.maxOutputTokens";
    let requested = match json::get(body, path) {
        Some(value @ Value::Number(_)) => json::int_of(Some(value)),
        _ => return,
    };
    let Some(info) = thinking::lookup(models, model, PROVIDER) else {
        return;
    };
    let limit = if info.output_token_limit > 0 {
        info.output_token_limit
    } else {
        info.max_completion_tokens
    };
    if limit == 0 || requested <= i64::try_from(limit).unwrap_or(i64::MAX) {
        return;
    }
    json::set(body, path, Value::from(limit));
}

impl ProviderExecutor for GeminiExecutor {
    fn id(&self) -> &str {
        PROVIDER
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

    /// Returns the credential as it is: an API key needs no refresh.
    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        let auth = Auth::clone(&auth);
        async move { Ok(auth) }.boxed()
    }
}

#[cfg(test)]
mod tests;
