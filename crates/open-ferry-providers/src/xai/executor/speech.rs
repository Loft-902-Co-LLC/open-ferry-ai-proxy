// Ported from CLIProxyAPI internal/runtime/executor/xai_executor_speech.go
// (xaiSpeechModelUnavailablePatterns, xaiSpeechModelUnavailable,
// xaiSpeechStatusErr, executeSpeech) and xai_executor_request.go
// (xaiSpeechRequestURL, xaiIsSpeechRequest) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! [`XaiExecutor`]'s speech calls (upstream's `executeSpeech`).
//!
//! A call from the speech endpoints (source format `openai-speech`) posts
//! its body, which the handler has already put in xAI's shape, to xAI's
//! text-to-speech API: `<base URL>/tts`, under the credential's base URL
//! (see [`super::super::request::base_url`]), or under xAI's API when that
//! is Grok's CLI chat proxy, which has no speech. The model is the body's
//! `model`, trimmed, else the request's; the config's payload rules apply
//! for that model, protocol `openai`, their defaults checked against the
//! payload as it came. The request has the credential's key and custom
//! headers and `Accept: */*`. xAI's answer, the audio, comes back with its
//! headers.
//!
//! A failure is an error with xAI's status and body (see
//! [`super::super::errors`]). A 404 is this request's alone, so the manager
//! neither cools the credential nor tries another, unless its body says the
//! model is unknown or unavailable (a `model_not_found` code, "unsupported
//! model" and the like), as an unknown voice's 404 is the request's fault
//! and an unknown model's the credential's.
//!
//! Deviations from upstream:
//! - A speech call with the `responses/compact` `alt`, which upstream
//!   compacts, is refused with a 400 before anything is sent, as an image
//!   or video one is; a streaming one gets the same refusal, where upstream
//!   says streaming isn't supported for `/responses/compact`. The speech
//!   endpoints make neither.
//! - The audio is read up to 256 MiB, and an error body up to 4 MiB;
//!   upstream reads both whole.
//! - A secret the request sent is redacted from xAI's error body, as from
//!   every xAI answer here (see the executor's module); the audio is passed
//!   on as it came.
//! - An error body serde can't read as JSON, though gjson could, is
//!   searched for the patterns as text; in one it can, a key given twice is
//!   read by its last copy, and an object at one of the paths is searched as
//!   compact JSON, where gjson's is the text as it came.
//! - Usage records and request logs come from the call's taps (see
//!   [`crate::observe_send`]): the record names the model, no tokens and no
//!   response model, as upstream's.
//! - Only API keys: upstream's OAuth credentials aren't served.

use bytes::Bytes;
use http::Method;
use http::header::{ACCEPT, HeaderValue};
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{ErrorKind, ExecError, Format, Options, Request, Response};
use open_ferry_core::observe::AttemptKind;
use open_ferry_translate::go;
use serde_json::Value;

use super::videos::{payload_model, shape};
use super::{MAX_ERROR_BODY, XaiExecutor};
use crate::codex::client::{error_chain, read_body, read_body_prefix};
use crate::codex::request::refuse_control_characters;
use crate::json::{get, str_of};
use crate::observe_send::{self, Attempt};
use crate::redact::Policy;
use crate::xai::errors;
use crate::xai::request::{
    CLI_CHAT_PROXY_BASE_URL, DEFAULT_BASE_URL, PROVIDER, base_url, build_headers,
};

/// The source format of upstream's speech handlers (`xaiSpeechHandlerType`).
const SPEECH_SOURCE: &str = "openai-speech";
/// xAI's text-to-speech path (`xaiTTSPath`).
const TTS_PATH: &str = "/tts";
/// The most audio read from one answer.
const MAX_AUDIO: usize = 256 << 20;

/// What in a 404's body says the model is unknown or unavailable, lower
/// case (`xaiSpeechModelUnavailablePatterns`).
const PATTERNS: [&str; 12] = [
    "model_not_found",
    "model_not_supported",
    "model is not supported",
    "model is unsupported",
    "model not supported",
    "unsupported model",
    "model is not available",
    "model not available",
    "model is unavailable",
    "model unavailable",
    "not available for your plan",
    "not available for your account",
];

/// Where in a JSON error body to look for them.
const PATHS: [&str; 8] = [
    "code",
    "error.code",
    "type",
    "error.type",
    "error",
    "error.message",
    "message",
    "detail",
];

/// Whether the call is from the speech endpoints (`xaiIsSpeechRequest`).
pub(super) fn is_speech_request(options: &Options) -> bool {
    options.source_format.as_str() == SPEECH_SOURCE
}

/// Where a speech call goes (`xaiSpeechRequestURL`): `/tts` under the
/// credential's base URL, or under xAI's API in place of Grok's CLI chat
/// proxy.
fn speech_url(auth: &Auth) -> String {
    let mut base = base_url(auth);
    if base.trim_end_matches('/') == CLI_CHAT_PROXY_BASE_URL {
        base = DEFAULT_BASE_URL;
    }
    let base = base.strip_suffix('/').unwrap_or(base);
    format!("{base}{TTS_PATH}")
}

/// Whether an error body says the model is unknown or unavailable
/// (`xaiSpeechModelUnavailable`).
fn model_unavailable(body: &[u8]) -> bool {
    let has_pattern = |text: &str| PATTERNS.iter().any(|pattern| text.contains(pattern));
    let parsed = if go::gjson_valid(body) {
        serde_json::from_slice::<Value>(body).ok()
    } else {
        None
    };
    match parsed {
        Some(parsed) => PATHS
            .iter()
            .any(|path| has_pattern(&go::to_lower(&str_of(get(&parsed, path))))),
        None => has_pattern(&go::to_lower(&String::from_utf8_lossy(body))),
    }
}

impl XaiExecutor {
    /// `Execute` for a speech call (`executeSpeech`; see the module docs).
    pub(super) async fn execute_speech(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        let model = payload_model(request);
        let (body, _) = shape(self, &model, request, options, |_| {});
        let url = speech_url(auth);
        refuse_control_characters(&url)?;
        let mut headers = build_headers(auth, &options.headers, false, "")?;
        headers.insert(ACCEPT, HeaderValue::from_static("*/*"));

        let secrets = observe_send::secrets(&url, &headers, &self.proxy_for(auth), auth);
        let format = Format::OPENAI_SPEECH;
        let method = Method::POST;
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
        let mut response = self
            .clients
            .get(&auth.proxy_url)
            .request(method, &url)
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
            let (raw, _) = read_body_prefix(response, MAX_ERROR_BODY).await;
            tracing::debug!(status, "xai: speech request error");
            let unavailable = model_unavailable(&raw);
            let body = secrets.bytes(&raw, Policy::Client);
            let mut error = errors::status_error(status, &body);
            if error.status == 404 && !unavailable {
                error.request_scoped = true;
            }
            return Err(error.into());
        }
        let response_headers = response.headers().clone();
        let audio = read_body(response, MAX_AUDIO).await.map_err(|error| {
            ExecError::new(
                ErrorKind::Upstream,
                secrets.text(error.to_string(), Policy::Client),
            )
        })?;
        Ok(Response {
            payload: Bytes::from(audio),
            headers: response_headers,
        })
    }
}

#[cfg(test)]
mod tests;
