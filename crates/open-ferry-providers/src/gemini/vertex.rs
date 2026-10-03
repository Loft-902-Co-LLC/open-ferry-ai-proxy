// Ported from CLIProxyAPI internal/runtime/executor/gemini_vertex_executor.go
// (GeminiVertexExecutor: Execute, ExecuteStream, CountTokens, Refresh and
// their service-account and API-key variants, isImagenModel,
// getVertexAction, convertImagenToGeminiResponse, convertToImagenRequest,
// vertexCreds, vertexAPICreds, vertexBaseURL) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! [`VertexExecutor`]: calls Gemini models on Vertex AI.
//!
//! A credential with an `api_key` attribute, or an `access_token` in its
//! metadata, calls `{base_url}/v1/publishers/google/models/{model}:{action}`
//! with the key as `x-goog-api-key`; the base URL is the credential's
//! `base_url` attribute or Google's global endpoint. Any other credential
//! must hold a service account (`project_id`, `location` and
//! `service_account` in its metadata), and calls the endpoint of its
//! location for its project with a bearer token from that account (see
//! [`super::token`]).
//!
//! Imagen models (any model whose name contains `imagen`) are called with
//! `predict`. With a service account, the client's request becomes an
//! Imagen request (its first text as the prompt) and Imagen's pictures come
//! back as a Gemini answer, so they reach the client as from any other
//! Gemini image model.
//!
//! Deviations from upstream, besides those in [`super`]:
//! - A credential with no metadata at all, rather than none for Go's nil
//!   map, is reported as missing its metadata.
//! - An Imagen request is written by `serde_json`, which leaves `<`, `>`
//!   and `&` as they are where Go escapes them, and a message whose content
//!   isn't a string gives that content as compact JSON for the prompt where
//!   upstream takes its text as sent.
//! - Refresh returns the credential as it is; the Home service isn't
//!   ported.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use http::HeaderMap;
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{ErrorKind, ExecError, Options, Request, Response, StreamResponse};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::models::ModelCatalog;
use open_ferry_translate::signature::sanitize_gemini_request_thought_signatures;
use serde_json::{Map, Value, json};

use super::stream::Lines;
use super::token::{ServiceAccount, TokenCache, service_account};
use super::{
    Credential, build_headers, post, prepare_count_body, read_answer, reject_compact,
    translate_answer, translate_count, translate_request, translate_stream, turns,
};
use crate::codex::client::Clients;
use crate::codex::request::{base_model, set_string_if_different};
use crate::codex::terminal::StatusError;
use crate::json;

/// How log lines and errors name this executor.
const NAME: &str = "vertex executor";
/// The provider whose models the executor looks up.
const PROVIDER: &str = "vertex";
/// The API version in request paths.
const API_VERSION: &str = "v1";
/// Vertex AI's global endpoint, for API keys.
const GLOBAL_BASE_URL: &str = "https://aiplatform.googleapis.com";
/// The location of a service account that names none.
const DEFAULT_LOCATION: &str = "us-central1";

/// Calls Gemini models on Vertex AI with service accounts or API keys
/// (upstream's `GeminiVertexExecutor`).
pub struct VertexExecutor {
    clients: Clients,
    models: Option<Arc<dyn ModelCatalog>>,
    tokens: TokenCache,
    /// Where service-account calls go instead of their location's endpoint,
    /// in tests.
    service_account_base_url: Option<String>,
}

/// How a credential reaches Vertex AI.
enum Target<'a> {
    /// `vertexAPICreds`: an API key, and a base URL that may be empty.
    ApiKey { key: &'a str, base_url: &'a str },
    /// `vertexCreds`: a service account of `project` at `location`.
    ServiceAccount {
        project: String,
        location: String,
        account: Box<ServiceAccount>,
    },
}

impl VertexExecutor {
    /// An executor whose credentials without a `proxy_url` go through
    /// `global_proxy_url`: empty for the environment's proxy, `direct` or
    /// `none` for no proxy, or an `http`, `https` or `socks5` proxy URL.
    /// Access tokens are fetched through the same proxy as the calls.
    pub fn new(global_proxy_url: impl Into<String>) -> Self {
        Self {
            clients: Clients::new(global_proxy_url).for_provider(NAME),
            models: None,
            tokens: TokenCache::default(),
            service_account_base_url: None,
        }
    }

    /// Looks up models in `models`, for their thinking support, before the
    /// built-in catalog.
    pub fn with_models(mut self, models: Arc<dyn ModelCatalog>) -> Self {
        self.models = Some(models);
        self
    }

    /// Sends service-account calls to `base_url` instead of Google's.
    #[cfg(test)]
    fn with_service_account_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.service_account_base_url = Some(base_url.into());
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
        turns::strip_vertex_tool_call_ids(&mut body, options.source_format.as_str());
        sanitize_gemini_request_thought_signatures(&mut body, "contents");
        turns::ensure_boundary_user(&mut body, "contents");
        json::delete(&mut body, "session_id");
        Ok(body)
    }

    /// The URL of `action` on `model` for `target`.
    fn url(&self, target: &Target<'_>, model: &str, action: &str) -> String {
        match target {
            Target::ApiKey { base_url, .. } => {
                let base_url = if base_url.is_empty() {
                    GLOBAL_BASE_URL
                } else {
                    base_url
                };
                format!("{base_url}/{API_VERSION}/publishers/google/models/{model}:{action}")
            }
            Target::ServiceAccount {
                project, location, ..
            } => {
                let base_url = self
                    .service_account_base_url
                    .clone()
                    .unwrap_or_else(|| vertex_base_url(location));
                format!(
                    "{base_url}/{API_VERSION}/projects/{project}/locations/{location}/publishers/google/models/{model}:{action}"
                )
            }
        }
    }

    /// The request headers for `target`. A service account's token is
    /// fetched here; failing that, the call fails with a 500 and the reason
    /// is only logged.
    async fn headers(
        &self,
        auth: &Auth,
        options: &Options,
        target: &Target<'_>,
    ) -> Result<HeaderMap, ExecError> {
        match target {
            Target::ApiKey { key, .. } => {
                build_headers(auth, options, &Credential::ApiKey(key), NAME)
            }
            Target::ServiceAccount { account, .. } => {
                let client = self.clients.get(&auth.proxy_url);
                let token = match self.tokens.token(&client, account).await {
                    Ok(token) => token,
                    Err(error) => {
                        tracing::error!("{NAME}: access token error: {error}");
                        return Err(StatusError::new(500, "internal server error").into());
                    }
                };
                build_headers(auth, options, &Credential::Bearer(&token), NAME)
            }
        }
    }

    async fn execute_inner(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        reject_compact(options)?;
        let target = target(auth)?;
        let base = base_model(&request.model);
        // Only a service account calls Imagen with its own request.
        let imagen = is_imagen(base) && matches!(target, Target::ServiceAccount { .. });
        let body = if imagen {
            convert_to_imagen_request(&request.payload)?
        } else {
            self.prepare(request, options, false)?
        };
        let mut url = self.url(&target, base, action(base, false));
        if !options.alt.is_empty() {
            url.push_str("?$alt=");
            url.push_str(&options.alt);
        }
        let headers = self.headers(auth, options, &target).await?;
        let client = self.clients.get(&auth.proxy_url);
        let response = post(&client, &url, headers, &body, NAME).await?;
        let (headers, mut data) = read_answer(response).await?;
        if imagen {
            data = convert_imagen_to_gemini_response(data, base);
        }
        translate_answer(request, options, &body, headers, data)
    }

    async fn execute_stream_inner(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<StreamResponse, ExecError> {
        reject_compact(options)?;
        let target = target(auth)?;
        let base = base_model(&request.model);
        let body = self.prepare(request, options, true)?;
        let mut url = self.url(&target, base, action(base, true));
        // Imagen doesn't stream, so it gets no SSE parameters.
        if !is_imagen(base) {
            if options.alt.is_empty() {
                url.push_str("?alt=sse");
            } else {
                url.push_str("?$alt=");
                url.push_str(&options.alt);
            }
        }
        let headers = self.headers(auth, options, &target).await?;
        let client = self.clients.get(&auth.proxy_url);
        let response = post(&client, &url, headers, &body, NAME).await?;
        Ok(translate_stream(
            response,
            request,
            options,
            &body,
            Lines::Raw,
            NAME,
        ))
    }

    async fn count_tokens_inner(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        let target = target(auth)?;
        let base = base_model(&request.model);
        let mut body = translate_request(request, options, false, self.models(), PROVIDER)?;
        turns::strip_vertex_tool_call_ids(&mut body, options.source_format.as_str());
        prepare_count_body(&mut body, base);
        let url = self.url(&target, base, "countTokens");
        let headers = self.headers(auth, options, &target).await?;
        let client = self.clients.get(&auth.proxy_url);
        let response = post(&client, &url, headers, &body, NAME).await?;
        let (headers, data) = read_answer(response).await?;
        Ok(translate_count(options, headers, data))
    }
}

/// How `auth` reaches Vertex AI: by API key when it has one
/// (`vertexAPICreds`), else by service account (`vertexCreds`).
fn target(auth: &Auth) -> Result<Target<'_>, ExecError> {
    let mut key = auth.attribute("api_key").unwrap_or_default();
    if key.is_empty() {
        key = auth.metadata_str("access_token").unwrap_or_default();
    }
    if !key.is_empty() {
        return Ok(Target::ApiKey {
            key,
            base_url: auth.attribute("base_url").unwrap_or_default(),
        });
    }
    service_account_target(&auth.metadata)
        .map_err(|message| ExecError::new(ErrorKind::Upstream, message))
}

/// `vertexCreds`: the project, location and service account in a
/// credential's `metadata`.
fn service_account_target(metadata: &Map<String, Value>) -> Result<Target<'static>, String> {
    if metadata.is_empty() {
        return Err(format!("{NAME}: missing auth metadata"));
    }
    let trimmed = |field: &str| {
        metadata
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
    };
    let project = match trimmed("project_id") {
        "" => trimmed("project"),
        project => project,
    };
    if project.is_empty() {
        return Err(format!("{NAME}: missing project_id in credentials"));
    }
    let location = match trimmed("location") {
        "" => DEFAULT_LOCATION,
        location => location,
    };
    let Some(Value::Object(fields)) = metadata.get("service_account") else {
        return Err(format!("{NAME}: missing service_account in credentials"));
    };
    let account = service_account(fields).map_err(|error| format!("{NAME}: {error}"))?;
    Ok(Target::ServiceAccount {
        project: project.to_owned(),
        location: location.to_owned(),
        account: Box::new(account),
    })
}

/// `vertexBaseURL`: the endpoint of `location`, which is Google's global
/// one for `global`.
fn vertex_base_url(location: &str) -> String {
    match location.trim() {
        "" => format!("https://{DEFAULT_LOCATION}-aiplatform.googleapis.com"),
        "global" => GLOBAL_BASE_URL.to_owned(),
        location => format!("https://{location}-aiplatform.googleapis.com"),
    }
}

/// `isImagenModel`.
fn is_imagen(model: &str) -> bool {
    model.to_lowercase().contains("imagen")
}

/// `getVertexAction`.
fn action(model: &str, stream: bool) -> &'static str {
    if is_imagen(model) {
        "predict"
    } else if stream {
        "streamGenerateContent"
    } else {
        "generateContent"
    }
}

/// `convertToImagenRequest`: an Imagen `predict` request for the client's
/// `payload`. The prompt is the first text of a Gemini request, else the
/// first message with content, else a `prompt` field;
/// `aspectRatio`, `sampleCount` and `negativePrompt` are taken as they
/// are.
fn convert_to_imagen_request(payload: &[u8]) -> Result<Value, ExecError> {
    let payload: Value = serde_json::from_slice(payload).unwrap_or(Value::Null);
    let mut prompt = json::get(&payload, "contents.0.parts.0.text")
        .map(|text| json::str_of(Some(text)))
        .unwrap_or_default();
    if prompt.is_empty()
        && let Some(Value::Array(messages)) = payload.get("messages")
    {
        prompt = messages
            .iter()
            .filter_map(|message| message.get("content"))
            .map(|content| json::str_of(Some(content)))
            .find(|content| !content.is_empty())
            .unwrap_or_default();
    }
    if prompt.is_empty() {
        prompt = json::str_of(payload.get("prompt"));
    }
    if prompt.is_empty() {
        return Err(ExecError::new(
            ErrorKind::Upstream,
            "imagen: no prompt found in request",
        ));
    }

    // Go writes the keys of its maps in order.
    let mut instance = Map::new();
    if let Some(negative) = payload.get("negativePrompt") {
        instance.insert(
            "negativePrompt".to_owned(),
            Value::from(json::str_of(Some(negative))),
        );
    }
    instance.insert("prompt".to_owned(), Value::from(prompt));
    let mut parameters = Map::new();
    if let Some(ratio) = payload.get("aspectRatio") {
        parameters.insert(
            "aspectRatio".to_owned(),
            Value::from(json::str_of(Some(ratio))),
        );
    }
    let samples = payload
        .get("sampleCount")
        .map_or(1, |count| json::int_of(Some(count)));
    parameters.insert("sampleCount".to_owned(), Value::from(samples));
    Ok(json!({
        "instances": [Value::Object(instance)],
        "parameters": Value::Object(parameters),
    }))
}

/// `convertImagenToGeminiResponse`: Imagen's `predictions` in `data` as a
/// Gemini answer from `model` with an inline picture for each, or `data`
/// as it is when it has no predictions.
fn convert_imagen_to_gemini_response(data: Vec<u8>, model: &str) -> Vec<u8> {
    let predictions = match serde_json::from_slice::<Value>(&data) {
        Ok(answer) => match answer.get("predictions") {
            Some(Value::Array(predictions)) => predictions.clone(),
            _ => return data,
        },
        Err(_) => return data,
    };
    let parts: Vec<Value> = predictions
        .iter()
        .filter_map(|prediction| {
            let image = json::str_of(prediction.get("bytesBase64Encoded"));
            if image.is_empty() {
                return None;
            }
            let mime_type = match json::str_of(prediction.get("mimeType")) {
                mime if mime.is_empty() => "image/png".to_owned(),
                mime => mime,
            };
            Some(json!({"inlineData": {"data": image, "mimeType": mime_type}}))
        })
        .collect();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    // Imagen gives no token counts.
    let answer = json!({
        "candidates": [{
            "content": {"parts": parts, "role": "model"},
            "finishReason": "STOP",
        }],
        "modelVersion": model,
        "responseId": format!("imagen-{nanos}"),
        "usageMetadata": {
            "candidatesTokenCount": 0,
            "promptTokenCount": 0,
            "totalTokenCount": 0,
        },
    });
    serde_json::to_vec(&answer).unwrap_or(data)
}

impl ProviderExecutor for VertexExecutor {
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

    /// Returns the credential as it is: an API key needs no refresh, and a
    /// service account's tokens are fetched as calls need them.
    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        let auth = Auth::clone(&auth);
        async move { Ok(auth) }.boxed()
    }
}

#[cfg(test)]
mod tests;
