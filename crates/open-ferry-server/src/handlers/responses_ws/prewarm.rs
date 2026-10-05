// Ported from shouldHandleResponsesWebsocketPrewarmLocally,
// normalizeResponsesWebsocketPrewarmFollowup and
// syntheticResponsesWebsocketPrewarmPayloads in CLIProxyAPI
// sdk/api/handlers/openai/openai_responses_websocket_prewarm.go (v8.0.15,
// MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Warm-ups: a `response.create` with `generate: false` asks only that the
//! connection be ready, so it is answered here, and its input goes with the
//! request that follows.

use std::time::{SystemTime, UNIX_EPOCH};

use uuid::Uuid;

use super::requests::{
    Normalized, TYPE_APPEND, TYPE_CREATE, bad_request, input_not_array, merge_input, request_type,
    transcript_replacement, unsupported_type,
};
use crate::errors::ErrorMessage;
use crate::json::{self, Val, str_at};

/// The `response.created` a warm-up is answered with.
const CREATED_TEMPLATE: &[u8] = br#"{"type":"response.created","sequence_number":0,"response":{"id":"","object":"response","created_at":0,"status":"in_progress","background":false,"error":null,"output":[]}}"#;

/// The `response.completed` a warm-up is answered with.
const COMPLETED_TEMPLATE: &[u8] = br#"{"type":"response.completed","sequence_number":1,"response":{"id":"","object":"response","created_at":0,"status":"completed","background":false,"error":null,"output":[],"usage":{"input_tokens":0,"input_tokens_details":{"cached_tokens":0},"output_tokens":0,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":0}}}"#;

/// Whether `raw` is a warm-up to answer here: a `response.create` with
/// `generate` false, unless the upstream gets requests as they are
/// (`shouldHandleResponsesWebsocketPrewarmLocally`).
pub(super) fn should_handle_locally(raw: &[u8], allow_incremental: bool) -> bool {
    if allow_incremental || request_type(raw) != TYPE_CREATE {
        return false;
    }
    json::get(raw, "generate").is_some_and(|generate| !generate.bool())
}

/// A request after a warm-up: the warm-up's input, which never reached the
/// upstream, then this one's (`normalizeResponsesWebsocketPrewarmFollowup`).
pub(super) fn normalize_followup(
    raw: &[u8],
    warmup_request: &[u8],
) -> Result<Normalized, ErrorMessage> {
    let kind = request_type(raw);
    if kind != TYPE_CREATE && kind != TYPE_APPEND {
        return Err(unsupported_type(&kind));
    }
    let Some(input) = json::get(raw, "input").filter(Val::is_array) else {
        return Err(input_not_array());
    };
    let merged = merge_input(warmup_request, b"[]", input.raw)
        .map_err(|err| bad_request(err.to_string()))?;
    let normalized = transcript_replacement(raw, warmup_request);
    let Some(normalized) = json::try_set_raw(&normalized, "input", &merged) else {
        return Err(bad_request(
            "cannot set array element for non-numeric key 'input'",
        ));
    };
    Ok((normalized.clone(), normalized))
}

/// The `response.created` and `response.completed` that answer a warm-up
/// (`syntheticResponsesWebsocketPrewarmPayloads`).
pub(super) fn synthetic_payloads(request: &[u8]) -> [Vec<u8>; 2] {
    let response_id = format!("resp_prewarm_{}", Uuid::now_v7());
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        });
    let model = str_at(request, "model");
    let model = model.trim();
    [CREATED_TEMPLATE, COMPLETED_TEMPLATE].map(|template| {
        let payload = json::set_str(template, "response.id", &response_id);
        let payload = json::set_raw(
            &payload,
            "response.created_at",
            created_at.to_string().as_bytes(),
        );
        if model.is_empty() {
            payload
        } else {
            json::set_str(&payload, "response.model", model)
        }
    })
}
