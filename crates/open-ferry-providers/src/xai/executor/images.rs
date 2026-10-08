// Ported from CLIProxyAPI internal/runtime/executor/xai_executor_media.go
// (executeImages), xai_executor_request.go (xaiImageEndpointPath) and the
// image dispatch of xai_executor_execute.go (Execute) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! [`XaiExecutor`]'s image calls (upstream's `executeImages`).
//!
//! A call from the image endpoints (source format `openai-image`) posts its
//! body to xAI's Images API under the credential's base URL (see
//! [`super::super::request::base_url`]): to `/images/edits` when the path
//! the client called ends so, else to `/images/generations`. The server's
//! image handlers write the body in xAI's shape and read the answer whole.
//!
//! The model is the body's `model`, trimmed, else the request's. The body is
//! shaped as a video call's is: its image references rewritten to xAI's
//! shape (see [`super::super::request::normalize_image_refs`]), then the
//! config's payload rules applied for that model, protocol `openai`, their
//! defaults checked against the request's payload as it came. The request
//! has the credential's key and custom headers, and asks for JSON. xAI's
//! answer comes back with its headers; a failure is an error with xAI's
//! status and body (see [`super::super::errors`]). The call's usage record
//! names the body's model, the model xAI's answer names, if any, and no
//! tokens, as upstream's (`ObserveResponseModel` and `EnsurePublished`).
//!
//! Deviations from upstream:
//! - A streaming image call is refused with a 400 before anything is sent;
//!   upstream sends it to the Responses API, which no image handler asks
//!   for. So is an image call with the `responses/compact` `alt`, which
//!   upstream compacts.
//! - The body is sent as it came unless the image references or a rule
//!   change it; a changed body is written compactly in its own key order,
//!   where upstream's rewrite of the references writes it with Go's sorted
//!   keys. A body that isn't a JSON object is sent as it came, without the
//!   rules.
//! - Error bodies are read up to 4 MiB, and a success's up to 50 MiB;
//!   upstream reads both whole.
//! - A secret the request sent is redacted from xAI's answer, success or
//!   failure, as from every xAI answer here (see the executor's module).
//! - Usage records and request logs come from the call's taps (see
//!   [`crate::observe_send`]).
//! - Only API keys: upstream's OAuth credentials, which go to Grok's CLI
//!   chat proxy unless they name a `base_url`, aren't served.

use http::Method;
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{ErrorKind, ExecError, Format, Options, Request, Response};
use open_ferry_core::observe::AttemptKind;

use super::videos::{payload_model, shape_body};
use super::{MAX_ERROR_BODY, XaiExecutor};
use crate::codex::client::{error_chain, read_body, read_body_prefix};
use crate::codex::request::refuse_control_characters;
use crate::codex::stream::MAX_LINE;
use crate::observe_send::{self, Attempt};
use crate::redact::Policy;
use crate::xai::errors;
use crate::xai::request::{PROVIDER, base_url, build_headers};

/// The source format of upstream's image handlers (`xaiImageHandlerType`).
const IMAGE_SOURCE: &str = "openai-image";
/// xAI's paths (`xaiImagesGenerationsPath` and `xaiImagesEditsPath`).
const GENERATIONS_PATH: &str = "/images/generations";
const EDITS_PATH: &str = "/images/edits";

/// The xAI path an image call goes to, if the call is from the image
/// endpoints (`xaiImageEndpointPath`): edits when the path the client
/// called ends so, else generations.
pub(super) fn endpoint(options: &Options) -> Option<&'static str> {
    if options.source_format.as_str() != IMAGE_SOURCE {
        return None;
    }
    if options.metadata.request_path.trim().ends_with(EDITS_PATH) {
        Some(EDITS_PATH)
    } else {
        Some(GENERATIONS_PATH)
    }
}

/// The URL of `path` under `base`, less one trailing slash.
fn url(base: &str, path: &str) -> String {
    format!("{}{path}", base.strip_suffix('/').unwrap_or(base))
}

impl XaiExecutor {
    /// `Execute` for an image call to xAI's `path` (`executeImages`; see
    /// the module docs).
    pub(super) async fn execute_images(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
        path: &str,
    ) -> Result<Response, ExecError> {
        let model = payload_model(request);
        let (body, _) = shape_body(self, &model, request, options);
        let url = url(base_url(auth), path);
        refuse_control_characters(&url)?;
        let headers = build_headers(auth, &options.headers, false, "")?;

        let secrets = observe_send::secrets(&url, &headers, &self.proxy_for(auth), auth);
        let format = Format::OPENAI_IMAGE;
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
                &attempt.request(&Method::POST, &url, &headers, &body, &secrets),
            )
        });
        let mut response = self
            .clients
            .get(&auth.proxy_url)
            .post(&url)
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

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let (body, _) = read_body_prefix(response, MAX_ERROR_BODY).await;
            tracing::debug!(status, "xai: image request error");
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
            payload: data.into(),
            headers: response_headers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: an image call goes to edits when the path the client
    // called ends so, trimmed as upstream reads it, and to generations
    // otherwise; another call isn't an image call.
    #[test]
    fn endpoints() {
        for (path, want) in [
            ("/v1/images/edits", EDITS_PATH),
            (" /v1/images/edits\n", EDITS_PATH),
            ("/v1/images/generations", GENERATIONS_PATH),
            ("/v1/images/edits/", GENERATIONS_PATH),
            ("", GENERATIONS_PATH),
        ] {
            let mut options = Options::new(Format::OPENAI_IMAGE);
            options.metadata.request_path = path.into();
            assert_eq!(endpoint(&options), Some(want), "{path:?}");
        }
        for format in [Format::OPENAI, Format::OPENAI_VIDEO, Format::CODEX] {
            let mut options = Options::new(format);
            options.metadata.request_path = "/v1/images/edits".into();
            assert_eq!(endpoint(&options), None);
        }
    }

    // Ported from TestXAIExecutorExecuteImagesOAuthBaseURLResolution's
    // API-key cases: an API key without a base URL goes to xAI's API, one
    // with a base URL to it. The OAuth cases aren't ported (only API keys
    // are served).
    #[test]
    fn image_calls_go_under_the_credentials_base_url() {
        let mut auth = Auth::default();
        auth.attributes
            .insert("api_key".into(), "xai-api-key".into());
        assert_eq!(
            url(base_url(&auth), GENERATIONS_PATH),
            "https://api.x.ai/v1/images/generations"
        );
        for base in [
            "https://custom-gateway.example.com/v1",
            "https://custom-gateway.example.com/v1/",
        ] {
            auth.attributes.insert("base_url".into(), base.into());
            assert_eq!(
                url(base_url(&auth), GENERATIONS_PATH),
                "https://custom-gateway.example.com/v1/images/generations"
            );
        }
    }
}
