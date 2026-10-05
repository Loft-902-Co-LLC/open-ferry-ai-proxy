// Ported from CLIProxyAPI internal/runtime/executor/xai_websockets_executor.go
// (buildXAIWebsocketRequestBody, xaiWebsocketGenerateFalse,
// buildXAIWebsocketWarmupCompletedPayload, buildXAIWebsocketCompactionPayload,
// validateXAIWebsocketCompactionResponse) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The `response.create` message, and what is made up for the client: the
//! completed response of a warmup, and the compaction a `compaction_trigger`
//! asks for.
//!
//! Deviations from upstream: none.

use std::time::SystemTime;

use serde_json::{Value, json};

use crate::codex::terminal::StatusError;
use crate::codex::usage::ensure_responses_usage_details;
use crate::json::{bool_of, delete, exists, get, int_of, set, str_at};
use crate::xai::compact;

/// What a compaction without its compacted state fails with.
const MISSING_COMPACTED_STATE: &str =
    "xai websocket compaction response is missing compacted state";

/// The usage of a warmup's completed response.
fn zero_usage() -> Value {
    json!({
        "input_tokens": 0,
        "input_tokens_details": {"cached_tokens": 0},
        "output_tokens": 0,
        "output_tokens_details": {"reasoning_tokens": 0},
        "total_tokens": 0,
    })
}

/// The message for the prepared `body` (`buildXAIWebsocketRequestBody`): a
/// `response.create`, stored, without the HTTP call's streaming fields, and
/// without instructions when it continues a previous response.
pub(super) fn request_message(body: &Value) -> Value {
    let mut message = body.clone();
    set(&mut message, "type", Value::from("response.create"));
    for field in ["stream", "stream_options", "background"] {
        delete(&mut message, field);
    }
    set(&mut message, "store", Value::Bool(true));
    if !str_at(&message, "previous_response_id").trim().is_empty() {
        delete(&mut message, "instructions");
    }
    message
}

/// Whether the message asks xAI not to generate (`xaiWebsocketGenerateFalse`):
/// a warmup, which xAI answers with `response.created` alone.
pub(super) fn generate_false(message: &Value) -> bool {
    exists(message, "generate") && !bool_of(get(message, "generate"))
}

/// The completed response the client gets for a warmup's
/// `response.created` event `created` (`buildXAIWebsocketWarmupCompletedPayload`):
/// its response, completed, with no output and no usage unless it had some.
pub(super) fn warmup_completed(created: &Value) -> Vec<u8> {
    let mut completed = json!({
        "type": "response.completed",
        "response": {"output": [], "usage": zero_usage()},
    });
    if let Some(sequence) = get(created, "sequence_number") {
        set(
            &mut completed,
            "sequence_number",
            Value::from(int_of(Some(sequence)).wrapping_add(1)),
        );
    }
    if let Some(response @ Value::Object(_)) = get(created, "response") {
        let mut response = response.clone();
        set(&mut response, "status", Value::from("completed"));
        if !exists(&response, "output") {
            set(&mut response, "output", json!([]));
        }
        if !exists(&response, "usage") {
            set(&mut response, "usage", zero_usage());
        }
        set(&mut completed, "response", response);
    }
    ensure_responses_usage_details(completed.to_string().into_bytes())
}

/// The compact call's payload: the client's `payload` with `input` as its
/// input and no previous response (`buildXAIWebsocketCompactionPayload`).
pub(super) fn compaction_payload(payload: &Value, input: Vec<Value>) -> Value {
    let mut out = payload.clone();
    set(&mut out, "input", Value::Array(input));
    delete(&mut out, "previous_response_id");
    out
}

/// The compact call's response ID and compaction item
/// (`validateXAIWebsocketCompactionResponse`), or a 502 when `data` isn't
/// JSON or has no compaction with encrypted content first in its output.
pub(super) fn validate_compaction(
    data: &[u8],
    now: SystemTime,
) -> Result<(String, Value), StatusError> {
    let compact: Value = serde_json::from_slice(data)
        .map_err(|_| StatusError::new(502, "xai websocket compaction returned invalid JSON"))?;
    let missing = || StatusError::new(502, MISSING_COMPACTED_STATE);
    let id_ok = matches!(get(&compact, "id"), Some(Value::String(id)) if !id.trim().is_empty());
    let Some(Value::Array(output)) = get(&compact, "output") else {
        return Err(missing());
    };
    if !id_ok {
        return Err(missing());
    }
    let item_ok = output.first().is_some_and(|item| {
        matches!(item, Value::Object(_))
            && matches!(get(item, "type"), Some(Value::String(kind)) if kind.trim() == "compaction")
            && matches!(
                get(item, "encrypted_content"),
                Some(Value::String(content)) if !content.trim().is_empty()
            )
    });
    if !item_ok {
        return Err(missing());
    }
    let response_id = compact::response_id(&compact, now);
    let item = compact::output_item(&compact, &response_id);
    Ok((response_id, item))
}
