// Ported from CLIProxyAPI internal/runtime/executor/gemini_executor.go
// (executeInteractions's answer, sanitizeGeminiInteractionsUnsupportedInputIDs,
// translateGeminiInteractionsRequestBody, translateGeminiInteractionsRequestPair,
// geminiInteractionsPayloadConfigSource, geminiInteractionsPayloadConfigInput,
// geminiInteractionsSameByteSlice, applyGeminiInteractionsRevisionHeader,
// applyGeminiInteractionsRequestHeaders) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The body and headers of a native Interactions call, and its answer.
//!
//! The client's request is translated to Interactions twice when the
//! client's original request differs from the payload the executor got:
//! once for the body, and once as the baseline the payload rules' defaults
//! are checked against. When they are the same bytes, it is translated once.
//! An Interactions client's request is copied as it is.
//!
//! Before the body goes, IDs the API rejects are removed from its `input`
//! steps: a `function_call` keeps its `id` (taken from its `call_id` when it
//! has none) and loses its `call_id`; every other step loses its `id`, and
//! every content part its `id`.
//!
//! The `Api-Revision` header is the credential's `header:Api-Revision`
//! attribute, else the client's, else `2026-05-20`.
//!
//! Deviations from upstream:
//! - A plugin's request hooks and the model the credential manager resolved
//!   for an API key (`APIKeyModelIsCompat`) aren't ported, so a request is
//!   translated as for a model that isn't a compatibility model.
//! - The payload and the client's original request count as the same when
//!   the original is empty or both are views of the same bytes, as
//!   upstream compares its slices' first byte; the translations are
//!   owned values, so no buffer is shared between them.

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue};
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{ExecError, Format, Options, Request, Response};
use open_ferry_translate::registry::{Registry, ResponseContext};
use serde_json::Value;

use crate::codex::compat;
use crate::codex::request::{original_request, parse_object, response_format};
use crate::codex::terminal::{APPLY_PATCH_ERROR_MESSAGE, StatusError};
use crate::codex::usage::ensure_responses_usage_details;
use crate::json;

/// Google's Gemini API.
const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";
/// The API version in request paths.
const API_VERSION: &str = "v1beta";
/// The API revision a request asks for when neither the credential nor the
/// client names one (upstream's `geminiInteractionsAPIRevision`).
pub(super) const API_REVISION: &str = "2026-05-20";
/// The header naming the API revision.
pub(super) const API_REVISION_HEADER: HeaderName = HeaderName::from_static("api-revision");

/// `geminiAPIKey`: the credential's `api_key` attribute, as it is.
pub(super) fn api_key(auth: &Auth) -> &str {
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

/// The Interactions endpoint for `auth`.
pub(super) fn interactions_url(auth: &Auth) -> String {
    format!("{}/{API_VERSION}/interactions", base_url(auth))
}

/// `translateGeminiInteractionsRequestBody`: `payload` from the client's
/// format to Interactions for `model`, as a stream or not. An Interactions
/// client's payload, or one of no format, is left as it is; a Codex
/// client's is readied as `config` says first.
pub(super) fn translate_body(
    config: Option<&Config>,
    options: &Options,
    model: &str,
    mut payload: Value,
    stream: bool,
) -> Value {
    let source = &options.source_format;
    if source.as_str().is_empty() || *source == Format::INTERACTIONS {
        return payload;
    }
    compat::before_translation(config, options, &Format::INTERACTIONS, &mut payload);
    Registry::global().translate_request(source, &Format::INTERACTIONS, model, payload, stream)
}

/// `translateGeminiInteractionsRequestPair`: the baseline the payload rules'
/// defaults check, the client's original request translated, and the body
/// to send, the payload translated. When both are the same bytes they are
/// translated once and the body is a copy; otherwise the payload is
/// translated first.
pub(super) fn translate_pair(
    config: Option<&Config>,
    request: &Request,
    options: &Options,
    model: &str,
    stream: bool,
) -> (Value, Value) {
    let source = payload_config_input(options, &request.payload);
    if same_bytes(&request.payload, source) {
        let original = translate_body(
            config,
            options,
            model,
            parse_object(&request.payload),
            stream,
        );
        let working = original.clone();
        return (original, working);
    }
    let working = translate_body(
        config,
        options,
        model,
        parse_object(&request.payload),
        stream,
    );
    let original = translate_body(config, options, model, parse_object(source), stream);
    (original, working)
}

/// `geminiInteractionsPayloadConfigInput`: the client's original request,
/// or the payload when there is none.
fn payload_config_input<'a>(options: &'a Options, payload: &'a Bytes) -> &'a Bytes {
    if options.original_request.is_empty() {
        payload
    } else {
        &options.original_request
    }
}

/// `geminiInteractionsSameByteSlice`: whether `a` and `b` view the same
/// bytes, compared by where they start and their length rather than by
/// content.
fn same_bytes(a: &Bytes, b: &Bytes) -> bool {
    a.len() == b.len() && (a.is_empty() || a.as_ptr() == b.as_ptr())
}

/// `sanitizeGeminiInteractionsUnsupportedInputIDs`: the IDs of `input`
/// steps and content parts that the API's schema rejects removed. A
/// `function_call` needs an `id` and takes no `call_id`; a
/// `function_result` needs a `call_id` and takes no `id`; other steps and
/// content parts take no `id`.
pub(super) fn sanitize_input_ids(body: &mut Value) {
    let Some(Value::Array(items)) = body.get_mut("input") else {
        return;
    };
    for item in items {
        if json::str_at(item, "type") == "function_call" {
            if !json::exists(item, "id")
                && let Some(call_id) = json::get(item, "call_id")
            {
                let call_id = json::str_of(Some(call_id));
                json::set(item, "id", Value::String(call_id));
            }
            json::delete(item, "call_id");
        } else {
            json::delete(item, "id");
        }
        if let Some(Value::Array(parts)) = item.get_mut("content") {
            for part in parts {
                json::delete(part, "id");
            }
        }
    }
}

/// `applyGeminiInteractionsRequestHeaders` and
/// `applyGeminiInteractionsRevisionHeader`: an `Api-Revision` the headers
/// don't already carry is the client's, else [`API_REVISION`].
pub(super) fn apply_api_revision(headers: &mut HeaderMap, client: &HeaderMap) {
    if has_value(headers.get(&API_REVISION_HEADER)) {
        return;
    }
    if let Some(revision) = client
        .get(&API_REVISION_HEADER)
        .filter(|value| !value.is_empty())
    {
        headers.insert(API_REVISION_HEADER, revision.clone());
        return;
    }
    headers.insert(API_REVISION_HEADER, HeaderValue::from_static(API_REVISION));
}

/// Whether a header is there with a value, as Go's `Header.Get` sees it.
fn has_value(value: Option<&HeaderValue>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

/// Translates the Interactions answer `data` to the request `sent` into the
/// client's format.
pub(super) fn translate_answer(
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
        .translate_non_stream(&Format::INTERACTIONS, &format, &context, data)
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

#[cfg(test)]
mod tests;
