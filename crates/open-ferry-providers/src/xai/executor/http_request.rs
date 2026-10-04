// Ported from CLIProxyAPI internal/runtime/executor/xai_executor.go
// (PrepareRequest, HttpRequest) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! [`XaiExecutor`]'s plain HTTP requests (upstream's `HttpRequest`).
//!
//! The request gets the credential's `api_key` as `Authorization: Bearer`,
//! or loses any `Authorization` when there is none, then the credential's
//! custom headers. A path goes under the credential's base URL, else xAI's
//! API.
//!
//! Deviations from upstream:
//! - The request goes through the credential's shared proxy client with
//!   rustls. Like every call here, it has a connect timeout; like
//!   upstream's, no overall one.
//! - The user agent is always `open-ferry/<version>`, and no Grok CLI
//!   identity header is sent, whatever the call or a custom header says.
//!   `x-grok-conv-id` is the call's own, if it has one; a custom header
//!   can't set it.
//! - The answer's body is read here, up to the call's limit, and the rest
//!   dropped; a connection error reads as no answer, without the URL.
//! - The URL is read as a WHATWG URL; one with an ASCII control character
//!   fails before anything is sent.
//! - An answer whose status isn't a success has every secret the request
//!   sent redacted from its body, as a client's error is (see
//!   [`observe_send::secrets`] and [`crate::redact`]). Upstream hands the
//!   body on as it came.

use bytes::Bytes;
use http::HeaderName;
use http::header::{self, HeaderValue};
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{ErrorKind, ExecError, Format, HttpCall, HttpReply, HttpTarget};
use open_ferry_core::observe::AttemptKind;

use super::XaiExecutor;
use crate::codex::client::{USER_AGENT, error_chain, read_body_prefix};
use crate::codex::request::refuse_control_characters;
use crate::custom_headers;
use crate::observe_send::{self, Attempt, BodyTap};
use crate::redact::Policy;
use crate::xai::request::{CONV_ID_HEADER, PROVIDER, base_url, strip_forbidden_headers, token};

impl XaiExecutor {
    /// Sends `call` with the credential's key and custom headers, and reads
    /// the answer, whatever its status (`HttpRequest`); a failure's body
    /// without the secrets the request sent.
    pub(super) async fn http_request_inner(
        &self,
        auth: &Auth,
        call: HttpCall,
    ) -> Result<HttpReply, ExecError> {
        let HttpCall {
            method,
            target,
            mut headers,
            body,
            client_headers,
            response_limit,
            observation,
        } = call;
        let url = match target {
            HttpTarget::Url(url) => url,
            HttpTarget::Path(path) => format!("{}{path}", base_url(auth).trim_end_matches('/')),
        };
        refuse_control_characters(&url)?;
        let token = token(auth);
        if token.is_empty() {
            headers.remove(header::AUTHORIZATION);
        } else {
            let mut value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
                ExecError::new(
                    ErrorKind::Upstream,
                    "xai executor: the credential's token isn't a valid header value",
                )
            })?;
            value.set_sensitive(true);
            headers.insert(header::AUTHORIZATION, value);
        }
        let conversation = headers.get(CONV_ID_HEADER).cloned();
        custom_headers::apply(&mut headers, &auth.attributes, &client_headers, PROVIDER);
        strip_forbidden_headers(&mut headers);
        headers.remove(CONV_ID_HEADER);
        if let Some(conversation) = conversation {
            headers.insert(HeaderName::from_static(CONV_ID_HEADER), conversation);
        }
        headers.insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
        let attempt = Attempt {
            observation: observation
                .as_ref()
                .filter(|observation| observation.is_tapped()),
            kind: AttemptKind::Http,
            provider: PROVIDER,
            model: "",
            format: &Format::CODEX,
            auth,
        };
        let secrets = observe_send::secrets(&url, &headers, &self.proxy_for(auth), auth);
        let tap = attempt.observation.map(|observation| {
            observe_send::announce(
                observation,
                &attempt.request(&method, &url, &headers, &body, &secrets),
            )
        });
        let mut response = self
            .clients
            .get(&auth.proxy_url)
            .request(method, url)
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
        let headers = response.headers().clone();
        let tap = BodyTap::of(&response);
        let (mut body, read_error) = read_body_prefix(response, response_limit).await;
        let read_error = read_error.map(|error| error_chain(&error));
        if let Some(error) = &read_error {
            observe_send::attempt_error(tap.as_ref(), error);
        }
        if !(200..300).contains(&status)
            && let std::borrow::Cow::Owned(scrubbed) = secrets.bytes(&body, Policy::Client)
        {
            body = scrubbed;
        }
        Ok(HttpReply {
            status,
            headers,
            body: Bytes::from(body),
            read_error,
        })
    }
}

#[cfg(test)]
mod tests;
