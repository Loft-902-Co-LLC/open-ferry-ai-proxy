// Ported from CLIProxyAPI internal/runtime/executor/xai_executor_media.go
// (executeVideos) and xai_executor_request.go (xaiIsVideoRequest,
// xaiVideoEndpointPath, xaiMetadataString) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI
//
// The download is modelled on CLIProxyAPI
// sdk/api/handlers/openai/openai_videos_handlers.go (writeVideoContentFromURL,
// videoContentHTTPClient) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! [`XaiExecutor`]'s video calls (upstream's `executeVideos`), and the
//! download of a finished video.
//!
//! A call from the video endpoints (source format `openai-video`) goes to
//! xAI's video API under the credential's base URL (see
//! [`super::super::request::base_url`]):
//! - a request path ending `/videos/generations`, `/videos/edits` or
//!   `/videos/extensions` posts the body to that path;
//! - otherwise a body with a `request_id` asks for that video's state, with
//!   a `GET /videos/<request_id>` and no body, the ID escaped as Go's
//!   `url.PathEscape` escapes it;
//! - otherwise the body is posted to `/videos/generations`.
//!
//! The model is the body's `model`, trimmed, else the request's. The body
//! has its image references rewritten to xAI's shape (see
//! [`super::super::request::normalize_image_refs`]), then the config's
//! payload rules applied for that model, protocol `openai`, their defaults
//! checked against the request's payload as it came. The request has the
//! credential's key and custom headers, and asks for JSON. A post carries
//! `x-idempotency-key`: the client's `Idempotency-Key`, trimmed, else its
//! own `x-idempotency-key`, trimmed. xAI's answer comes back with its
//! headers; a failure is an error with xAI's status and body (see
//! [`super::super::errors`]). Each call's usage record names that model,
//! the model xAI's answer names, if any, and no tokens, as upstream's
//! (`ObserveResponseModel` and `EnsurePublished`): a create, a retrieve and
//! the lookup before a download each make one, a failure as a failure.
//!
//! A download ([`ProviderExecutor::download`]) fetches a URL xAI gave with
//! a plain `GET`: no key, no custom header and no body, through the proxy
//! of the credential that made the video, or the global proxy without one.
//! A success streams its body as it comes, never held whole; any other
//! status comes back with its body read up to 4 MiB.
//!
//! Deviations from upstream:
//! - A streaming video call is refused with a 400 before anything is sent,
//!   as before; upstream sends it to the Responses API, which no video
//!   handler asks for. So is a video call with the `responses/compact`
//!   `alt`, which upstream compacts.
//! - The body is sent as it came unless the image references or a rule
//!   change it; a changed body is written compactly in its own key order,
//!   where upstream's rewrite of the references writes it with Go's sorted
//!   keys. A body that isn't a JSON object is sent as it came, without the
//!   rules.
//! - Error bodies are read up to 4 MiB, and a success's up to 50 MiB;
//!   upstream reads both whole.
//! - A secret the request sent is redacted from xAI's answer, success or
//!   failure, as from every xAI answer here (see the executor's module).
//!   A download's error body is redacted of its URL's key-like query
//!   parameters, the proxy's password and the credential's keys; its
//!   success body, the video, is passed on as it came.
//! - Usage records and request logs come from the call's taps (see
//!   [`crate::observe_send`]); a download isn't tapped, as upstream's
//!   handler doesn't record it.
//! - A download goes through the executor's shared clients (see
//!   [`crate::codex::client`]), so it follows a redirect only within the
//!   URL's origin, where upstream's handler follows one anywhere, and says
//!   `User-Agent: open-ferry/<version>`. A URL with an ASCII control
//!   character fails before anything is sent. The status text is the
//!   status's canonical reason, where Go's is the one the server sent.
//! - Only API keys: upstream's OAuth credentials, which go to Grok's CLI
//!   chat proxy unless they name a `base_url`, aren't served.
//!
//! [`ProviderExecutor::download`]: open_ferry_core::executor::ProviderExecutor::download

use bytes::Bytes;
use futures_util::StreamExt as _;
use http::Method;
use http::header::{HeaderName, HeaderValue};
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{Downloaded, ErrorKind, ExecError, Format, Options, Request, Response};
use open_ferry_core::observe::AttemptKind;
use open_ferry_translate::json::exact;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;

use super::{MAX_ERROR_BODY, XaiExecutor};
use crate::codex::client::{error_chain, read_body, read_body_prefix};
use crate::codex::request::refuse_control_characters;
use crate::codex::stream::MAX_LINE;
use crate::json::str_at;
use crate::observe_send::{self, Attempt};
use crate::payload;
use crate::redact::{Policy, Secrets};
use crate::xai::errors;
use crate::xai::request::{PROVIDER, base_url, build_headers, normalize_image_refs};

/// The source format of upstream's video handlers (`xaiVideoHandlerType`).
const VIDEO_SOURCE: &str = "openai-video";
/// xAI's paths (`xaiVideosGenerationsPath` and the rest).
const GENERATIONS_PATH: &str = "/videos/generations";
const EDITS_PATH: &str = "/videos/edits";
const EXTENSIONS_PATH: &str = "/videos/extensions";
const VIDEOS_PATH: &str = "/videos";
/// The header a post's idempotency key goes in.
const IDEMPOTENCY_HEADER: &str = "x-idempotency-key";

/// What Go's `url.PathEscape` leaves alone besides letters and digits:
/// the unreserved marks and the reserved characters a path segment may
/// hold (`shouldEscape` with `encodePathSegment`).
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~')
    .remove(b'$')
    .remove(b'&')
    .remove(b'+')
    .remove(b':')
    .remove(b'=')
    .remove(b'@');

/// Whether the call is from the video endpoints (`xaiIsVideoRequest`).
pub(super) fn is_video_request(options: &Options) -> bool {
    options.source_format.as_str() == VIDEO_SOURCE
}

/// The native video path the request path names, if any
/// (`xaiVideoEndpointPath`).
fn native_path(options: &Options) -> Option<&'static str> {
    if !is_video_request(options) {
        return None;
    }
    let path = options.metadata.request_path.trim();
    [EDITS_PATH, EXTENSIONS_PATH, GENERATIONS_PATH]
        .into_iter()
        .find(|native| path.ends_with(native))
}

/// `text` as Go's `url.PathEscape` gives it.
fn path_escape(text: &str) -> String {
    utf8_percent_encode(text, PATH_SEGMENT).to_string()
}

/// The method, URL and body of a video call to `base`.
fn endpoint(base: &str, options: &Options, body: Bytes, sent: &Value) -> (Method, String, Bytes) {
    let base = base.strip_suffix('/').unwrap_or(base);
    if let Some(path) = native_path(options) {
        return (Method::POST, format!("{base}{path}"), body);
    }
    let request_id = str_at(sent, "request_id");
    let request_id = request_id.trim();
    if request_id.is_empty() {
        (Method::POST, format!("{base}{GENERATIONS_PATH}"), body)
    } else {
        let url = format!("{base}{VIDEOS_PATH}/{}", path_escape(request_id));
        (Method::GET, url, Bytes::new())
    }
}

/// The model of a media call: the body's `model`, trimmed, else the
/// request's, trimmed.
pub(super) fn payload_model(request: &Request) -> String {
    let model = exact::from_slice(&request.payload)
        .map(|payload| str_at(&payload, "model").trim().to_owned())
        .unwrap_or_default();
    if model.is_empty() {
        request.model.trim().to_owned()
    } else {
        model
    }
}

/// The body to send: `payload` with its image references rewritten and the
/// payload rules applied, or as it came when neither changes it or it isn't
/// a JSON object. Also the body as JSON, or `Null`, for the routing.
pub(super) fn shape_body(
    executor: &XaiExecutor,
    model: &str,
    request: &Request,
    options: &Options,
) -> (Bytes, Value) {
    shape(executor, model, request, options, normalize_image_refs)
}

/// The body to send: `payload` changed by `rewrite`, then the payload rules
/// for `model`, protocol `openai`, applied, their defaults checked against
/// the payload as it came (upstream's `NewPayloadFinalizer` with protocol
/// `openai`); or as it came when neither changes it or it isn't a JSON
/// object. Also the body as JSON, or `Null`.
pub(super) fn shape(
    executor: &XaiExecutor,
    model: &str,
    request: &Request,
    options: &Options,
    rewrite: fn(&mut Value),
) -> (Bytes, Value) {
    let original = match exact::from_slice(&request.payload) {
        Ok(original @ Value::Object(_)) => original,
        _ => return (request.payload.clone(), Value::Null),
    };
    let mut body = original.clone();
    rewrite(&mut body);
    let target = payload::Target {
        executor: PROVIDER,
        protocol: &Format::OPENAI,
        model,
        root: "",
        stream: false,
        tracked: &[],
        translate: Some(&|_| original.clone()),
    };
    payload::apply(
        executor.config.as_deref(),
        &target,
        request,
        options,
        &mut body,
    );
    if body == original {
        (request.payload.clone(), body)
    } else {
        (Bytes::from(body.to_string()), body)
    }
}

/// The idempotency key a post carries: the client's `Idempotency-Key`, else
/// its `x-idempotency-key`, trimmed, if not empty.
fn idempotency_key(options: &Options) -> Option<String> {
    let metadata = options
        .metadata
        .idempotency_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty());
    let header = || {
        options
            .headers
            .get(IDEMPOTENCY_HEADER)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).trim().to_owned())
            .filter(|key| !key.is_empty())
    };
    metadata.map(str::to_owned).or_else(header)
}

/// A status line's text, as Go's `Response.Status` gives it, with the
/// status's canonical reason.
fn status_text(status: http::StatusCode) -> String {
    match status.canonical_reason() {
        Some(reason) => format!("{} {reason}", status.as_u16()),
        None => status.as_u16().to_string(),
    }
}

impl XaiExecutor {
    /// `Execute` for a video call (`executeVideos`; see the module docs).
    pub(super) async fn execute_videos(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        let model = payload_model(request);
        let (body, sent) = shape_body(self, &model, request, options);
        let (method, url, body) = endpoint(base_url(auth), options, body, &sent);
        refuse_control_characters(&url)?;
        let mut headers = build_headers(auth, &options.headers, false, "")?;
        if method == Method::POST
            && let Some(key) = idempotency_key(options)
        {
            let value = HeaderValue::from_str(&key).map_err(|_| {
                ExecError::upstream(
                    400,
                    "xai executor: the idempotency key isn't a valid header value",
                )
            })?;
            headers.insert(HeaderName::from_static(IDEMPOTENCY_HEADER), value);
        }

        let secrets = observe_send::secrets(&url, &headers, &self.proxy_for(auth), auth);
        let format = Format::OPENAI_VIDEO;
        let attempt = Attempt::new(
            options,
            AttemptKind::Execute,
            PROVIDER,
            &model,
            &format,
            auth,
        );
        let tap = attempt.observation.map(|observation| {
            observe_send::announce(
                observation,
                &attempt.request(&method, &url, &headers, &body, &secrets),
            )
        });
        let mut call = self
            .clients
            .get(&auth.proxy_url)
            .request(method.clone(), &url)
            .headers(headers);
        if method == Method::POST {
            call = call.body(body);
        }
        let mut response = call.send().await.map_err(|error| {
            ExecError::new(
                ErrorKind::Upstream,
                secrets.text(error_chain(&error.without_url()), Policy::Client),
            )
        })?;
        observe_send::response(tap, &mut response);

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let (body, _) = read_body_prefix(response, MAX_ERROR_BODY).await;
            tracing::debug!(status, "xai: video request error");
            let body = secrets.bytes(&body, Policy::Client);
            return Err(errors::status_error(status, &body).into());
        }
        let response_headers = response.headers().clone();
        let data = read_body(response, MAX_LINE).await.map_err(|error| {
            ExecError::new(
                ErrorKind::Upstream,
                secrets.text(error.to_string(), Policy::Client),
            )
        })?;
        let data = secrets.bytes(&data, Policy::Client).into_owned();
        Ok(Response {
            payload: Bytes::from(data),
            headers: response_headers,
        })
    }

    /// Fetches `url` for [`ProviderExecutor::download`] (see the module
    /// docs).
    ///
    /// [`ProviderExecutor::download`]: open_ferry_core::executor::ProviderExecutor::download
    pub(super) async fn download_inner(
        &self,
        auth: Option<&Auth>,
        url: &str,
    ) -> Result<Downloaded, ExecError> {
        refuse_control_characters(url).map_err(|error| error.with_status(502))?;
        let proxy_url = auth.map_or("", |auth| auth.proxy_url.as_str());
        let mut secrets = Secrets::new();
        secrets.add_url(url);
        secrets.add_proxy(self.clients.effective_proxy(proxy_url));
        if let Some(auth) = auth {
            secrets.add_auth(auth);
        }
        let response = self
            .clients
            .get(proxy_url)
            .get(url)
            .send()
            .await
            .map_err(|error| {
                ExecError::new(
                    ErrorKind::Upstream,
                    secrets.text(error_chain(&error.without_url()), Policy::Client),
                )
                .with_status(502)
            })?;
        let status = response.status();
        let headers = response.headers().clone();
        if !status.is_success() {
            let (body, _) = read_body_prefix(response, MAX_ERROR_BODY).await;
            let body = secrets.bytes(&body, Policy::Client).into_owned();
            let chunks: Vec<Result<Bytes, ExecError>> = if body.is_empty() {
                Vec::new()
            } else {
                vec![Ok(Bytes::from(body))]
            };
            return Ok(Downloaded {
                status: status.as_u16(),
                status_text: status_text(status),
                headers,
                body: futures_util::stream::iter(chunks).boxed(),
            });
        }
        let body = response
            .bytes_stream()
            .map(move |chunk| {
                chunk.map_err(|error| {
                    ExecError::new(
                        ErrorKind::Upstream,
                        secrets.text(error_chain(&error.without_url()), Policy::Client),
                    )
                })
            })
            .boxed();
        Ok(Downloaded {
            status: status.as_u16(),
            status_text: status_text(status),
            headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests;
