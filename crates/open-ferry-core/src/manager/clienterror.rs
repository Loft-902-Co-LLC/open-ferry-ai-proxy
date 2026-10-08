// Ported from CLIProxyAPI internal/clienterror/client_error.go (IsRequestFault,
// IsItemNotPersisted, IsClaudeThreadNotFound and the body checks) (v8.0.15,
// MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Whether an upstream failure is the client request's fault, which only the
//! client can fix, so retrying with another credential can't help.
//!
//! The server keeps a private copy of this for the Responses WebSocket; the
//! two should become one.
//!
//! Deviations from upstream:
//! - A JSON body with a key twice is read by its last value, where gjson
//!   takes the first.

use serde_json::Value;

use super::text::{go_lower, str_of};

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
/// fault (`IsRequestFault`). The caller passes the error's own status when
/// it has one.
pub fn is_request_fault(status: u16, text: &str) -> bool {
    // Payment and rate limits are the credential's, whatever the body says.
    if status == 402 || status == 429 {
        return false;
    }
    if status == 401 && body_has(text, &TYPE_PATHS, |kind| kind == "authentication_error") {
        return false;
    }
    // Claude's missing thread state comes from a stale continuation, not the
    // credential: the client replays the whole conversation.
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
pub fn is_item_not_persisted(message: &str) -> bool {
    let lower = go_lower(message);
    lower.contains("item with id")
        && lower.contains("not found")
        && lower.contains("items are not persisted when `store` is set to false")
}

/// Whether a failure with `status` and error text `text` is Claude's 404
/// for a stale `previous_message_id` continuation, whose thread state is
/// gone (`IsClaudeThreadNotFound`): a JSON error of that type and message,
/// or, from v8.0.20, a plain-text message that names both.
pub fn is_claude_thread_not_found(status: u16, text: &str) -> bool {
    if status != 404 {
        return false;
    }
    let body = text.trim();
    if body.is_empty() {
        return false;
    }
    let Ok(root) = serde_json::from_str::<Value>(body) else {
        let lower = go_lower(body);
        return lower.contains("thread state") && lower.contains("previous_message_id");
    };
    let kind = str_of(get_path(&root, "error.type"));
    let message = go_lower(&str_of(get_path(&root, "error.message")));
    kind.trim().eq_ignore_ascii_case("not_found_error")
        && message.contains("thread state")
        && message.contains("previous_message_id")
}

/// Whether `text` is a JSON body with a value at one of `paths` that, lower
/// cased and trimmed, satisfies `matches`.
fn body_has(text: &str, paths: &[&str], matches: impl Fn(&str) -> bool) -> bool {
    let body = text.trim();
    if body.is_empty() {
        return false;
    }
    let Ok(root) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    paths.iter().any(|path| {
        let value = str_of(get_path(&root, path));
        matches(go_lower(&value).trim())
    })
}

/// The value at a dotted path of object keys.
pub(crate) fn get_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(root, |value, key| value.as_object()?.get(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_faults_follow_upstream() {
        assert!(is_request_fault(400, "bad"));
        assert!(is_request_fault(
            0,
            r#"{"error":{"code":"context_length_exceeded"}}"#
        ));
        assert!(is_request_fault(500, r#"{"type":"invalid_request_error"}"#));
        assert!(!is_request_fault(
            429,
            r#"{"error":{"type":"invalid_request_error"}}"#
        ));
        assert!(!is_request_fault(
            400,
            r#"{"error":{"code":"model_not_found"}}"#
        ));
        assert!(!is_request_fault(
            401,
            r#"{"error":{"type":"authentication_error","code":"invalid_request_error"}}"#
        ));
        assert!(is_request_fault(
            404,
            "Item with id 'rs_1' not found. Items are not persisted when `store` is set to false."
        ));
        assert!(!is_request_fault(404, "not found"));
        assert!(!is_request_fault(503, "{"));
    }

    // The Claude cases of TestIsRequestFault. Upstream's "in response body"
    // case reads the body an error carries beside its text; a call's error
    // carries the provider's body as its text.
    #[test]
    fn claude_missing_thread_state_is_the_request_fault() {
        assert!(is_request_fault(
            404,
            r#"{"type":"error","error":{"type":"not_found_error","message":"No thread state was found for the requested previous_message_id. Replay the full conversation with thread create to start a new Thread."}}"#
        ));
        assert!(is_request_fault(
            404,
            r#"{"type":"error","error":{"type":"not_found_error","message":"No thread state was found for the requested previous_message_id."}}"#
        ));
        // Generic Claude not found.
        assert!(!is_request_fault(
            404,
            r#"{"error":{"type":"not_found_error","message":"Not Found"}}"#
        ));
        // v8.0.20's "Claude missing thread state plain text".
        assert!(is_request_fault(
            404,
            "No thread state was found for the requested previous_message_id. Replay the full conversation with thread create to start a new Thread."
        ));
        // Claude missing thread on server error.
        assert!(!is_request_fault(
            500,
            r#"{"error":{"type":"not_found_error","message":"No thread state was found for the requested previous_message_id."}}"#
        ));
        // Not upstream's: the type is matched ignoring case and surrounding
        // space, the message ignoring case.
        assert!(is_claude_thread_not_found(
            404,
            r#" {"error":{"type":" Not_Found_Error ","message":"THREAD STATE gone for PREVIOUS_MESSAGE_ID"}} "#
        ));
    }
}
