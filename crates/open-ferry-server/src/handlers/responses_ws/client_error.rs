// Ported from IsRequestFault, IsItemNotPersisted, hasModelNotFoundErrorBody,
// hasAuthenticationErrorBody and hasRequestFaultBody in CLIProxyAPI
// internal/clienterror/client_error.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Whether an upstream failure is the client request's fault, which only the
//! client can fix. A private copy until the package is ported for the
//! credential manager; Claude's missing thread state is the manager's check.

use open_ferry_core::manager::clienterror::is_claude_thread_not_found;
use open_ferry_translate::go;

use crate::json;

/// Codes that mean the request is at fault (`requestFaultCodes`).
const REQUEST_FAULT_CODES: [&str; 9] = [
    "cyber_policy",
    "context_length_exceeded",
    "message_too_big",
    "string_above_max_length",
    "invalid_prompt",
    "invalid_value",
    "unsupported_value",
    "invalid_request_error",
    "previous_response_not_found",
];

/// Types that mean the request is at fault (`requestFaultTypes`).
const REQUEST_FAULT_TYPES: [&str; 4] = [
    "invalid_request",
    "invalid_request_error",
    "bad_request_error",
    "invalid_prompt",
];

/// Where an error body may carry its code.
const CODE_PATHS: [&str; 4] = [
    "error.code",
    "code",
    "response.error.code",
    "body.error.code",
];

/// Where an error body may carry its type.
const TYPE_PATHS: [&str; 4] = [
    "error.type",
    "type",
    "response.error.type",
    "body.error.type",
];

/// Whether a failure with `status` and error text `text` is the request's
/// fault (`IsRequestFault`).
pub(super) fn is_request_fault(status: u16, text: &str) -> bool {
    // Payment and rate limits are the credential's, whatever the body says.
    if status == 402 || status == 429 {
        return false;
    }
    if status == 401 && body_has(text, &TYPE_PATHS, |kind| kind == "authentication_error") {
        return false;
    }
    if is_claude_thread_not_found(status, text) {
        return true;
    }
    // A credential that can't serve the model isn't the caller's fault.
    if body_has(text, &CODE_PATHS, |code| {
        code == "model_not_found" || code == "model_not_found_error"
    }) {
        return false;
    }
    if body_has(text, &CODE_PATHS, |code| {
        REQUEST_FAULT_CODES.contains(&code)
    }) || body_has(text, &TYPE_PATHS, |kind| {
        REQUEST_FAULT_TYPES.contains(&kind)
    }) {
        return true;
    }
    if is_item_not_persisted(text) {
        return true;
    }
    matches!(status, 400 | 409 | 413 | 422)
}

/// Whether `message` is the 404 for an item the upstream never stored
/// because `store` was false (`IsItemNotPersisted`).
fn is_item_not_persisted(message: &str) -> bool {
    let lower = go::to_lower(message);
    lower.contains("item with id")
        && lower.contains("not found")
        && lower.contains("items are not persisted when `store` is set to false")
}

/// Whether `text` is a JSON body with a value at one of `paths` that, lower
/// cased and trimmed, satisfies `matches`.
fn body_has(text: &str, paths: &[&str], matches: impl Fn(&str) -> bool) -> bool {
    let body = text.trim().as_bytes();
    if body.is_empty() || !json::valid(body) {
        return false;
    }
    paths.iter().any(|path| {
        let value = json::get(body, path)
            .map(|value| value.str())
            .unwrap_or_default();
        matches(go::to_lower(&value).trim())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The Claude cases of TestIsRequestFault.
    #[test]
    fn claude_missing_thread_state_is_the_request_fault() {
        let missing = r#"{"type":"error","error":{"type":"not_found_error","message":"No thread state was found for the requested previous_message_id."}}"#;
        assert!(is_request_fault(404, missing));
        assert!(!is_request_fault(500, missing));
        assert!(!is_request_fault(
            404,
            r#"{"error":{"type":"not_found_error","message":"Not Found"}}"#
        ));
    }
}
