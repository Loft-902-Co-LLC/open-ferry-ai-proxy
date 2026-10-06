// Ported from CLIProxyAPI internal/runtime/executor/openai_compat_executor.go
// (executeImages, executeImagesStream, openAICompatImageEndpointPath and the
// image dispatch of Execute and ExecuteStream) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The OpenAI Images endpoints through an OpenAI-compatible provider.
//!
//! A call from `/v1/images/generations` or `/v1/images/edits` (source
//! format `openai-image`) goes to `<base_url>/images/edits` when the path
//! the client called ends in `/images/edits`, else to
//! `<base_url>/images/generations`. Its body goes as the client sent it,
//! with the model of the call without its suffix and with `stream` set or
//! removed, a form written again (see the crate's `images` module), and
//! then the config's payload rules for images. No thinking setting,
//! translation or `prompt_cache_key` applies. The answer goes back as it
//! came, whole or as a stream.
//!
//! An error status fails the call with the provider's body as the message.
//! A 429 of a call that doesn't stream waits as the provider's other
//! errors do (see `status_error`); a stream's error names no wait, as
//! upstream's.
//!
//! Deviations from upstream:
//! - The request sends the client's `User-Agent`, or open-ferry's, as the
//!   provider's other requests do; upstream sends `cli-proxy-openai-compat`.
//! - An answer that doesn't stream is read up to 50 MiB and an error body
//!   up to 4 MiB, where upstream reads them whole. A stream is passed on a
//!   line at a time, where upstream passes on what each 32 KiB read gives.
//!   Each has the request's secrets redacted, as the provider's other
//!   answers do.
//! - A base URL with an ASCII control character fails before the body is
//!   prepared, where upstream's fails after.

use std::borrow::Cow;

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, header};
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{
    ErrorKind, ExecError, Format, Options, Request, Response, StreamResponse,
};
use open_ferry_core::observe::AttemptKind;

use super::super::status::status_error;
use super::{
    MAX_ERROR_BODY, NAME, OpenAiCompatExecutor, api_key, base_headers, base_url, stream_headers,
};
use crate::codex::client::{error_chain, read_body, read_body_prefix};
use crate::codex::request::base_model;
use crate::codex::stream::MAX_LINE;
use crate::codex::terminal::StatusError;
use crate::custom_headers;
use crate::images;
use crate::observe_send::{self, Attempt, BodyTap};
use crate::payload::{self, MediaTarget};
use crate::redact::{Policy, Secrets};

/// Where a generation goes, and any image call that isn't an edit.
const GENERATIONS: &str = "/images/generations";
/// Where an edit goes.
const EDITS: &str = "/images/edits";

/// Where a call goes, if it is from the OpenAI Images endpoints
/// (`openAICompatImageEndpointPath`): [`EDITS`] for a call to a path that
/// ends so, else [`GENERATIONS`].
pub(super) fn endpoint(options: &Options) -> Option<&'static str> {
    if options.source_format != Format::OPENAI_IMAGE {
        return None;
    }
    if options.metadata.request_path.trim().ends_with(EDITS) {
        Some(EDITS)
    } else {
        Some(GENERATIONS)
    }
}

/// An image call ready to send.
struct Call {
    url: String,
    headers: HeaderMap,
    body: Bytes,
    /// What the request sends that is scrubbed from what is kept of it and
    /// from its answers.
    secrets: Secrets,
}

impl OpenAiCompatExecutor {
    /// `Execute` for the OpenAI Images endpoints (`executeImages`).
    pub(super) async fn execute_images(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
        path: &str,
    ) -> Result<Response, ExecError> {
        let (response, secrets) = self.send_image(auth, request, options, path, false).await?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        if !(200..300).contains(&status) {
            let body = error_body(response, status, &secrets).await?;
            return Err(status_error(status, &headers, &body).into());
        }
        let data = read_body(response, MAX_LINE).await.map_err(|error| {
            ExecError::new(
                ErrorKind::Upstream,
                secrets.text(error.to_string(), Policy::Client),
            )
        })?;
        let data = match secrets.bytes(&data, Policy::Client) {
            Cow::Owned(redacted) => redacted,
            Cow::Borrowed(_) => data,
        };
        Ok(Response {
            payload: Bytes::from(data),
            headers,
        })
    }

    /// `ExecuteStream` for the OpenAI Images endpoints
    /// (`executeImagesStream`).
    pub(super) async fn execute_images_stream(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
        path: &str,
    ) -> Result<StreamResponse, ExecError> {
        let (response, secrets) = self.send_image(auth, request, options, path, true).await?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = error_body(response, status, &secrets).await?;
            return Err(StatusError::new(status, String::from_utf8_lossy(&body)).into());
        }
        Ok(StreamResponse {
            headers: response.headers().clone(),
            chunks: images::raw_stream(response, secrets),
        })
    }

    /// Prepares and posts an image call, as a stream or not, and returns
    /// the provider's answer, whatever its status, with the secrets the
    /// request sent.
    async fn send_image(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
        path: &str,
        stream: bool,
    ) -> Result<(reqwest::Response, Secrets), ExecError> {
        let model = base_model(&request.model);
        let call = self.prepare_image(auth, request, options, model, path, stream)?;
        let kind = if stream {
            AttemptKind::Stream
        } else {
            AttemptKind::Execute
        };
        let response = self
            .post(
                auth,
                &call.url,
                &call.headers,
                call.body,
                &call.secrets,
                Attempt::new(
                    options,
                    kind,
                    &self.provider,
                    model,
                    &Format::OPENAI_IMAGE,
                    auth,
                ),
            )
            .await?;
        Ok((response, call.secrets))
    }

    /// The image call for `request`, to `path` with `model`
    /// (`executeImages` up to the call): its body, with the payload rules
    /// applied, sent with its content type, the API key, the `User-Agent`,
    /// for a stream `Accept: text/event-stream` and `Cache-Control:
    /// no-cache`, and then the credential's custom headers.
    fn prepare_image(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
        model: &str,
        path: &str,
        stream: bool,
    ) -> Result<Call, ExecError> {
        let base_url = base_url(auth)?;
        let client_type = options
            .headers
            .get(header::CONTENT_TYPE)
            .map(|value| String::from_utf8_lossy(value.as_bytes()))
            .unwrap_or_default();
        let (body, content_type) =
            images::prepare_payload(&request.payload, model, &client_type, stream)?;
        let content_type = if content_type.is_empty() {
            "application/json".to_owned()
        } else {
            content_type
        };
        let (body, content_type) = payload::apply_media(
            Some(&*self.config),
            &MediaTarget {
                executor: &self.provider,
                model,
                protocol: &Format::OPENAI,
            },
            request,
            options,
            body,
            &content_type,
        )
        .map_err(|error| ExecError::new(ErrorKind::Upstream, error.to_string()))?;
        let content_type = HeaderValue::from_str(&content_type).map_err(|_| {
            ExecError::new(
                ErrorKind::Upstream,
                format!("{NAME}: the request's content type isn't a valid header value"),
            )
        })?;
        let mut headers = base_headers(api_key(auth), &options.headers, content_type)?;
        if stream {
            stream_headers(&mut headers);
        }
        custom_headers::apply(&mut headers, &auth.attributes, &options.headers, NAME);
        let url = format!("{}{path}", base_url.strip_suffix('/').unwrap_or(base_url));
        let secrets = observe_send::secrets(
            &url,
            &headers,
            self.clients.effective_proxy(&auth.proxy_url),
            auth,
        );
        Ok(Call {
            url,
            headers,
            body,
            secrets,
        })
    }
}

/// The body of an answer with the error `status`, redacted, or the error
/// reading it.
async fn error_body(
    response: reqwest::Response,
    status: u16,
    secrets: &Secrets,
) -> Result<Vec<u8>, ExecError> {
    let tap = BodyTap::of(&response);
    let (body, error) = read_body_prefix(response, MAX_ERROR_BODY).await;
    if let Some(error) = error {
        let error = ExecError::new(
            ErrorKind::Upstream,
            secrets.text(error_chain(&error.without_url()), Policy::Client),
        );
        observe_send::attempt_error(tap.as_ref(), &error);
        return Err(error);
    }
    tracing::debug!(status, "{NAME}: image request error");
    Ok(secrets.bytes(&body, Policy::Client).into_owned())
}
