// Ported from CLIProxyAPI internal/runtime/executor/gemini_executor.go and
// gemini_vertex_executor.go (the parts the two executors share)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Calls to Gemini: [`GeminiExecutor`] for the Gemini API with API keys, and
//! [`VertexExecutor`] for Vertex AI with API keys or service accounts.
//!
//! Both translate the client's request to Gemini's `generateContent`
//! format, apply the thinking setting of the model's suffix or the request
//! (`thinking`), and adjust the body as Gemini needs it: the
//! aspect ratio of `gemini-2.5-flash-image-preview` (`image`), the
//! request's `model`, Claude thought signatures that Gemini would reject,
//! and empty user turns around the model's (`turns`). The answer is
//! translated back to the client's format; a stream line by line
//! (`stream`).
//!
//! Requests carry the client's `User-Agent`, else this project's, and the
//! credential's `header:` attributes. Nothing else identifies the caller:
//! no `x-goog-api-client` or other header of Google's own clients is sent,
//! and a `header:x-goog-api-client` attribute is dropped with a warning.
//!
//! Left out, as for the other providers:
//! - Gemini's Interactions API and the `gemini-interactions` provider:
//!   requests always go to `generateContent`.
//! - AI Studio, which upstream serves through a websocket relay.
//! - Usage reporting and request logging.
//! - The Home service (its credential options and refresh).
//! - The config's `payload` rules.
//! - The model that the credential manager resolved for an API key
//!   (`APIKeyModelIsCompat`, `ResolvedModelInfo`): requests are translated
//!   as for a model that isn't a compatibility model. Codex clients'
//!   requests are readied for translation as the Codex `compat` module
//!   says.
//! - A `countTokens` action in the request metadata, which upstream lets
//!   `Execute` count tokens with.
//! - `PrepareRequest` and `HttpRequest`, which sign arbitrary requests for
//!   the management API.
//!
//! Deviations from upstream, for both executors:
//! - Requests go through `reqwest` with rustls, one shared client per proxy;
//!   `reqwest` adds `Accept: */*` to a request that sets no `Accept`. Error
//!   bodies are read up to 4 MiB and answers up to 50 MiB.
//! - A request without a client `User-Agent` carries this project's, where
//!   upstream sends Go's default.
//! - A `header:x-goog-api-client` attribute isn't applied, so a credential
//!   can't pass for one of Google's client libraries; upstream sets it.
//! - The body is written as `serde_json` writes it, compact.
//! - A dropped call or stream stops at once; upstream checks its context.
//! - An error body that quotes the API key or access token the request
//!   carried has it redacted (see the crate's `redact` module).

mod executor;
mod image;
mod sse;
mod stream;
#[cfg(test)]
mod testing;
pub(crate) mod thinking;
mod token;
mod turns;
mod vertex;

pub use executor::GeminiExecutor;
pub use vertex::VertexExecutor;

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, header};
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{
    ErrorKind, ExecError, Format, Options, Request, Response, StreamResponse,
};
use open_ferry_core::models::ModelCatalog;
use open_ferry_translate::go::trim_space;
use open_ferry_translate::registry::{Registry, ResponseContext};
use open_ferry_translate::signature::sanitize_gemini_request_thought_signatures;
use serde_json::Value;

use self::stream::{Lines, StreamSetup};
use crate::codex::client::{USER_AGENT, error_chain, read_body, read_body_prefix};
use crate::codex::compat;
use crate::codex::request::{
    base_model, original_request, parse_object, response_format, set_string_if_different,
};
use crate::codex::stream::MAX_LINE;
use crate::codex::terminal::{APPLY_PATCH_ERROR_MESSAGE, StatusError};
use crate::codex::usage::ensure_responses_usage_details;
use crate::custom_headers;
use crate::json::{self, Body};
use crate::redact;

/// The `alt` of a `/responses/compact` call, which Gemini can't serve.
const COMPACT_ALT: &str = "responses/compact";
/// How much of an error body is read.
const MAX_ERROR_BODY: usize = 4 << 20;
/// Gemini's API key header.
const API_KEY_HEADER: HeaderName = HeaderName::from_static("x-goog-api-key");
/// The header Google's own client libraries name themselves in, which is
/// never sent.
const GOOGLE_CLIENT_HEADER: &str = "x-goog-api-client";

/// Fails a `/responses/compact` call with a 501.
fn reject_compact(options: &Options) -> Result<(), ExecError> {
    if options.alt == COMPACT_ALT {
        return Err(StatusError::new(501, "/responses/compact not supported").into());
    }
    Ok(())
}

/// The client's request in Gemini's format for the model without its
/// suffix, as a stream or not, with its thinking setting applied and the
/// image aspect ratio fixed. Models are looked up as `provider` registered
/// them in `models`; a Codex client's request is readied as `config` says.
fn translate_request(
    config: Option<&Config>,
    request: &Request,
    options: &Options,
    stream: bool,
    models: Option<&dyn ModelCatalog>,
    provider: &str,
) -> Result<Value, ExecError> {
    let base = base_model(&request.model);
    let from = &options.source_format;
    let mut payload = parse_object(&request.payload);
    compat::before_translation(config, options, &Format::GEMINI, &mut payload);
    let mut body =
        Registry::global().translate_request(from, &Format::GEMINI, base, payload, stream);
    thinking::apply_request(
        &mut body,
        &request.model,
        from.as_str(),
        &Body::parse(&request.payload),
        &Body::parse(&options.original_request),
        models,
        provider,
    )?;
    image::fix_image_aspect_ratio(base, &mut body);
    Ok(body)
}

/// Readies a translated body for counting tokens: no tools, generation
/// settings or safety settings, the model without its suffix, no thought
/// signatures Gemini would reject, and an empty user turn before a first
/// model turn.
fn prepare_count_body(body: &mut Value, base: &str) {
    for field in ["tools", "generationConfig", "safetySettings"] {
        json::delete(body, field);
    }
    set_string_if_different(body, "model", base);
    sanitize_gemini_request_thought_signatures(body, "contents");
    turns::ensure_leading_user(body, "contents");
}

/// How a request proves who it is for.
enum Credential<'a> {
    /// `x-goog-api-key`, unless empty.
    ApiKey(&'a str),
    /// `Authorization: Bearer`, unless empty.
    Bearer(&'a str),
}

/// The request headers: a JSON body, the credential, the client's
/// `User-Agent` or this project's, and the credential's custom headers
/// (`applyGeminiHeaders`).
fn build_headers(
    auth: &Auth,
    options: &Options,
    credential: &Credential<'_>,
    name: &str,
) -> Result<HeaderMap, ExecError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let (header_name, value) = match credential {
        Credential::ApiKey(key) if !key.is_empty() => (API_KEY_HEADER, (*key).to_owned()),
        Credential::Bearer(token) if !token.is_empty() => {
            (header::AUTHORIZATION, format!("Bearer {token}"))
        }
        _ => (API_KEY_HEADER, String::new()),
    };
    if !value.is_empty() {
        let mut value = HeaderValue::from_str(&value).map_err(|_| {
            ExecError::new(
                ErrorKind::Upstream,
                format!("{name}: the credential isn't a valid header value"),
            )
        })?;
        value.set_sensitive(true);
        headers.insert(header_name, value);
    }
    let user_agent = options
        .headers
        .get(header::USER_AGENT)
        .map(|value| trim_space(value.as_bytes()))
        .filter(|value| !value.is_empty())
        .and_then(|value| HeaderValue::from_bytes(value).ok())
        .unwrap_or_else(|| HeaderValue::from_static(USER_AGENT));
    headers.insert(header::USER_AGENT, user_agent);
    custom_headers::apply(&mut headers, &auth.attributes, &options.headers, name);
    if headers.remove(GOOGLE_CLIENT_HEADER).is_some() {
        tracing::warn!(
            "{name}: dropped the {GOOGLE_CLIENT_HEADER} header, which names Google's own clients"
        );
    }
    Ok(headers)
}

/// The API key and bearer token `headers` carry, to keep out of errors.
fn sent_secrets(headers: &HeaderMap) -> [String; 2] {
    let key = headers
        .get(API_KEY_HEADER)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .unwrap_or_default();
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_owned();
    [key, token]
}

/// Posts `body` and returns the answer if its status is a success, else
/// the status and body as an error, without the credential it was sent
/// with.
async fn post(
    client: &reqwest::Client,
    url: &str,
    headers: HeaderMap,
    body: &Value,
    name: &str,
) -> Result<reqwest::Response, ExecError> {
    let [key, token] = sent_secrets(&headers);
    let response = client
        .post(url)
        .headers(headers)
        .body(body.to_string())
        .send()
        .await
        .map_err(|error| ExecError::new(ErrorKind::Upstream, error_chain(&error.without_url())))?;
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(response);
    }
    let (body, _) = read_body_prefix(response, MAX_ERROR_BODY).await;
    tracing::debug!(status, "{name}: request error");
    let body = redact::bytes(&body, &key);
    let body = redact::bytes(&body, &token);
    Err(StatusError::new(status, String::from_utf8_lossy(&body)).into())
}

/// The headers and body of a successful answer.
async fn read_answer(response: reqwest::Response) -> Result<(HeaderMap, Vec<u8>), ExecError> {
    let headers = response.headers().clone();
    let data = read_body(response, MAX_LINE)
        .await
        .map_err(|error| ExecError::new(ErrorKind::Upstream, error.to_string()))?;
    Ok((headers, data))
}

/// Translates Gemini's answer `data` to the request `sent` into the
/// client's format.
fn translate_answer(
    request: &Request,
    options: &Options,
    sent: &Value,
    headers: HeaderMap,
    data: Vec<u8>,
) -> Result<Response, ExecError> {
    let format = response_format(options);
    let original = original_request(request, options);
    let context = ResponseContext {
        model: &request.model,
        original_request: &original,
        request: sent,
    };
    let out = Registry::global()
        .translate_non_stream(&Format::GEMINI, &format, &context, data)
        .filter(|out| !out.is_empty())
        .ok_or_else(|| StatusError::new(502, APPLY_PATCH_ERROR_MESSAGE))?;
    let payload = if format == Format::OPENAI_RESPONSE {
        ensure_responses_usage_details(out)
    } else {
        out
    };
    Ok(Response {
        payload: Bytes::from(payload),
        headers,
    })
}

/// Translates Gemini's stream in `response`, to the request `sent` with
/// `secrets` (see [`sent_secrets`]), into the client's format.
fn translate_stream(
    response: reqwest::Response,
    request: &Request,
    options: &Options,
    sent: &Value,
    secrets: [String; 2],
    lines: Lines,
    name: &'static str,
) -> StreamResponse {
    let headers = response.headers().clone();
    let format = response_format(options);
    let original = original_request(request, options);
    let translator = Registry::global().response_stream(
        &Format::GEMINI,
        &format,
        &ResponseContext {
            model: &request.model,
            original_request: &original,
            request: sent,
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
        lines,
        secrets,
        name,
    };
    StreamResponse {
        headers,
        chunks: stream::translate(response, setup),
    }
}

/// Gemini's token count in `data`, in the client's format.
fn translate_count(options: &Options, headers: HeaderMap, data: Vec<u8>) -> Response {
    let count = serde_json::from_slice::<Value>(&data)
        .map(|answer| json::int_at(&answer, "totalTokens"))
        .unwrap_or_default();
    let payload = Registry::global().translate_token_count(
        &Format::GEMINI,
        &response_format(options),
        count,
        data,
    );
    Response {
        payload: Bytes::from(payload),
        headers,
    }
}
