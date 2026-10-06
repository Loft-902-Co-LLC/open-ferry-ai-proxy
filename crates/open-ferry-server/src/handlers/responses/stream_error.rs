// Ported from truncateResponsesStreamErrorText, redactResponsesStreamErrorText,
// sanitizeResponsesStreamEventName, isResponsesStreamSensitiveKey,
// sanitizeResponsesStreamErrorNode, responsesStreamErrorText and
// sanitizeResponsesStreamErrorMessage in CLIProxyAPI
// sdk/api/handlers/openai/openai_responses_handlers.go, and from
// BuildOpenAIResponsesStreamErrorChunk, BuildOpenAIResponsesStreamFailedChunk
// and the error classes in sdk/api/handlers/openai_responses_stream_error.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Errors in a Responses stream: their text with secrets taken out, and the
//! `error` and `response.failed` events that carry them.
//!
//! Deviations from upstream:
//! - An error's text that is a JSON object serde_json can't read, though Go
//!   can, is reported as its status's text alone. Such an object has an
//!   escaped lone surrogate, or is nested more than 128 deep. Go redacts it
//!   field by field; matching it with the patterns, as upstream does for
//!   text that isn't JSON, would miss escaped keys and keys like `password`.
//! - An error's text that is a JSON array is redacted field by field, as an
//!   object's fields are, then by the patterns and cut. Upstream only
//!   matches it with the patterns. An array serde_json can't read is
//!   reported as its status's text.

use std::sync::LazyLock;

use open_ferry_translate::go;
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::errors::ErrorMessage;
use crate::json::{sorted, sorted_map};
use crate::status::status_text;

/// How many characters of an error's text, or of a string in it, are kept.
const MESSAGE_LIMIT: usize = 2048;

/// How many characters of an event name are kept.
const FIELD_LIMIT: usize = 256;

/// A secret written as `key=value` or `"key":"value"`. Go's `\s` is ASCII
/// white space, so the classes spell it out.
static SENSITIVE_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)((?:"?(?:api[_-]?key|access[_-]?token|token|authorization|secret)"?)[\t\n\f\r ]*[=:][\t\n\f\r ]*"?)([^\t\n\f\r "&,;}]+)"#,
    )
    .expect("the pattern is valid")
});

/// A bearer token. Go's `\b` is an ASCII word boundary.
static BEARER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?-u:\b)Bearer[\t\n\f\r ]+[A-Za-z0-9._~+/=-]+").expect("the pattern is valid")
});

/// `text` cut to `limit` characters, with `…` when something was cut
/// (`truncateResponsesStreamErrorText`).
pub(super) fn truncate(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// `text` with secrets and bearer tokens replaced by `[REDACTED]`
/// (`redactResponsesStreamErrorText`).
pub(super) fn redact(text: &str) -> String {
    let text = SENSITIVE_VALUE.replace_all(text, "${1}[REDACTED]");
    BEARER.replace_all(&text, "Bearer [REDACTED]").into_owned()
}

/// An event name as diagnostics show it (`sanitizeResponsesStreamEventName`).
pub(super) fn sanitize_event_name(name: &str) -> String {
    truncate(&redact(name.trim()), FIELD_LIMIT)
}

/// Whether a field's value is a secret (`isResponsesStreamSensitiveKey`).
/// Token counts and limits aren't.
fn is_sensitive_key(key: &str) -> bool {
    let key = go::to_lower(key.trim()).replace('-', "_");
    if ["tokens", "token_count", "token_limit", "token_usage"]
        .iter()
        .any(|counter| key.contains(counter))
    {
        return false;
    }
    matches!(
        key.as_str(),
        "authorization"
            | "secret"
            | "password"
            | "passwd"
            | "api_key"
            | "apikey"
            | "token"
            | "access_token"
            | "refresh_token"
            | "id_token"
            | "auth_token"
            | "session_token"
            | "api_token"
            | "client_secret"
            | "client_key"
    ) || ["_secret", "_password", "_api_key", "_token"]
        .iter()
        .any(|suffix| key.ends_with(suffix))
}

/// `value` with secrets redacted and strings cut, its objects' keys in Go's
/// order (`sanitizeResponsesStreamErrorNode`).
fn sanitize_node(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(truncate(&redact(text), MESSAGE_LIMIT)),
        Value::Object(fields) => {
            let mut entries: Vec<(&String, &Value)> = fields.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, item)| {
                        let item = if is_sensitive_key(key) {
                            Value::String("[REDACTED]".to_owned())
                        } else {
                            sanitize_node(item)
                        };
                        (key.clone(), item)
                    })
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sanitize_node).collect()),
        other => other.clone(),
    }
}

/// The text a stream error is reported with (`responsesStreamErrorText`). A
/// JSON object keeps its error, or all of it, with secrets redacted; other
/// text is redacted and cut, and an array is redacted field by field, then
/// cut. An object or array serde_json can't read is reported as the status's
/// text, since it can't be redacted field by field.
pub(crate) fn stream_error_text(error: &ErrorMessage, status: u16) -> String {
    let text = match error.text.trim() {
        "" => status_text(status),
        text => text,
    };
    let plain = || truncate(&redact(text), MESSAGE_LIMIT);
    if !go::json_valid(text.as_bytes()) {
        return plain();
    }
    // Go decodes the text into a map, which takes an object or `null` and
    // turns anything else down.
    let root = match text.as_bytes().first() {
        Some(b'{') => match serde_json::from_str::<Map<String, Value>>(text) {
            Ok(root) => root,
            Err(_) => return status_text(status).to_owned(),
        },
        Some(b'n') => Map::new(),
        Some(b'[') => {
            return match serde_json::from_str::<Value>(text) {
                Ok(array) => truncate(&redact(&sanitize_node(&array).to_string()), MESSAGE_LIMIT),
                Err(_) => status_text(status).to_owned(),
            };
        }
        _ => return plain(),
    };
    let error_node = match root.get("error") {
        Some(Value::Object(_)) => root.get("error"),
        _ => root
            .get("response")
            .and_then(|response| response.get("error"))
            .filter(|node| node.is_object()),
    };
    if let Some(node) = error_node {
        let mut out = Map::new();
        out.insert("error".to_owned(), sanitize_node(node));
        if let Some(sequence) = root.get("sequence_number") {
            out.insert("sequence_number".to_owned(), sorted(sequence));
        }
        return Value::Object(out).to_string();
    }
    sanitize_node(&Value::Object(root)).to_string()
}

/// `error` as a stream reports it: a status from 400 to 599, otherwise 500,
/// and its text from [`stream_error_text`] (`sanitizeResponsesStreamErrorMessage`).
pub(crate) fn sanitize_error(error: ErrorMessage) -> ErrorMessage {
    let status = match error.status {
        status @ 400..=599 => status,
        _ => 500,
    };
    let text = stream_error_text(&error, status);
    ErrorMessage {
        status,
        text,
        ..error
    }
}

/// The OpenAI error code and type for a status
/// (`openAIResponsesStreamErrorClassFor`).
fn error_class(status: u16) -> (&'static str, &'static str) {
    match status {
        401 => ("invalid_api_key", "invalid_request_error"),
        403 => ("insufficient_quota", "invalid_request_error"),
        429 => ("rate_limit_exceeded", "invalid_request_error"),
        404 => ("model_not_found", "invalid_request_error"),
        408 => ("request_timeout", "server_error"),
        500.. => ("internal_server_error", "server_error"),
        400.. => ("invalid_request_error", "invalid_request_error"),
        _ => ("unknown_error", "invalid_request_error"),
    }
}

/// `text` as a JSON object, as Go decodes it into a map; `None` for
/// anything else, `null` included.
fn decode_object(text: &str) -> Option<Map<String, Value>> {
    if text.is_empty() || !go::json_valid(text.as_bytes()) {
        return None;
    }
    match serde_json::from_str(text) {
        Ok(Value::Object(payload)) => Some(payload),
        _ => None,
    }
}

/// The `error` and `sequence_number` of an `error` event
/// (`BuildOpenAIResponsesStreamErrorChunk`).
fn error_parts(status: u16, err_text: &str, sequence: i64) -> (Value, i64) {
    let status = if status == 0 { 500 } else { status };
    let mut sequence = sequence.max(0);
    let trimmed = err_text.trim();
    let message = if trimmed.is_empty() {
        status_text(status)
    } else {
        trimmed
    };
    let payload = decode_object(trimmed);
    if let Some(Value::Number(n)) = payload.as_ref().and_then(|p| p.get("sequence_number"))
        && let Ok(n) = n.to_string().parse::<i64>()
    {
        sequence = n;
    }
    let (code, _) = error_class(status);
    (
        error_detail(status, payload.as_ref(), code, message),
        sequence,
    )
}

/// The `error` object of an event (`openAIResponsesStreamErrorDetail`): the
/// payload's own error, or one built for the status with what the payload
/// says.
fn error_detail(
    status: u16,
    payload: Option<&Map<String, Value>>,
    code: &str,
    message: &str,
) -> Value {
    let mut code = code.to_owned();
    let mut message = message.to_owned();
    if let Some(payload) = payload {
        if let Some(node @ Value::Object(_)) = payload.get("error") {
            return sorted(node);
        }
        if let Some(node @ Value::Object(_)) = payload
            .get("response")
            .filter(|response| response.is_object())
            .and_then(|response| response.get("error"))
        {
            return sorted(node);
        }
        if let Some(Value::String(m)) = payload.get("message")
            && !m.trim().is_empty()
        {
            m.trim().clone_into(&mut message);
        }
        match payload.get("code") {
            None | Some(Value::Null) => {}
            Some(Value::String(c)) if !c.trim().is_empty() => c.trim().clone_into(&mut code),
            Some(other) => code = go_sprint(other).trim().to_owned(),
        }
    }
    let (_, kind) = error_class(status);
    let mut detail = Map::new();
    detail.insert("code".to_owned(), Value::String(code));
    detail.insert("message".to_owned(), Value::String(message));
    detail.insert("param".to_owned(), Value::Null);
    detail.insert("type".to_owned(), Value::String(kind.to_owned()));
    if let Some(payload) = payload {
        if let Some(Value::String(t)) = payload.get("type")
            && !t.trim().is_empty()
            && t.trim() != "error"
        {
            detail.insert("type".to_owned(), Value::String(t.trim().to_owned()));
        }
        if let Some(param) = payload.get("param") {
            detail.insert("param".to_owned(), sorted(param));
        }
    }
    Value::Object(detail)
}

/// The `error` event for a stream error
/// (`BuildOpenAIResponsesStreamErrorChunk`).
pub(super) fn error_chunk(status: u16, err_text: &str, sequence: i64) -> String {
    let (error, sequence) = error_parts(status, err_text, sequence);
    json!({"type": "error", "error": error, "sequence_number": sequence}).to_string()
}

/// The `response.failed` event Codex clients get for a stream error
/// (`BuildOpenAIResponsesStreamFailedChunk`).
pub(super) fn failed_chunk(status: u16, err_text: &str, sequence: i64) -> String {
    let (error, sequence) = error_parts(status, err_text, sequence);
    json!({
        "type": "response.failed",
        "sequence_number": sequence,
        "response": {"status": "failed", "error": error},
    })
    .to_string()
}

/// A decoded JSON value as Go's `fmt.Sprint` writes it.
fn go_sprint(value: &Value) -> String {
    match value {
        Value::Null => "<nil>".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(go_sprint).collect();
            format!("[{}]", items.join(" "))
        }
        Value::Object(fields) => {
            let fields: Vec<String> = sorted_map(fields)
                .iter()
                .map(|(key, item)| format!("{key}:{}", go_sprint(item)))
                .collect();
            format!("map[{}]", fields.join(" "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(status: u16, text: &str) -> String {
        stream_error_text(&ErrorMessage::new(status, text), status)
    }

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn redacts_and_truncates() {
        assert_eq!(
            redact(r#"token=abc "api_key": "def" Authorization: Bearer xyz.1 bearer  q"#),
            r#"token=[REDACTED] "api_key": "[REDACTED]" Authorization: [REDACTED] xyz.1 Bearer [REDACTED]"#
        );
        assert_eq!(redact("abearer x"), "abearer x");
        assert_eq!(redact("-Bearer x"), "-Bearer [REDACTED]");
        assert_eq!(truncate("héllo", 2), "hé…");
        assert_eq!(truncate("héllo", 5), "héllo");
        assert_eq!(sanitize_event_name("  ev  "), "ev");
    }

    #[test]
    fn recognises_sensitive_keys() {
        for key in [
            "Access-Token",
            " api_key ",
            "x_secret",
            "my-password",
            "client_key",
        ] {
            assert!(is_sensitive_key(key), "{key}");
        }
        for key in [
            "input_tokens",
            "token_limit",
            "max_token_count",
            "key",
            "message",
        ] {
            assert!(!is_sensitive_key(key), "{key}");
        }
    }

    // TestResponsesStreamErrorTextSanitizesNestedSensitiveFieldsAndDotKeys
    #[test]
    fn error_text_sanitizes_nested_sensitive_fields_and_dot_keys() {
        let out = text(
            400,
            r#"{"error":{"metadata":{"message":"Bearer test-secret"},"vendor.detail":"Bearer test-secret","access_token":"test-secret","normal_key":"safe_value"},"sequence_number":3}"#,
        );
        assert_eq!(
            out,
            r#"{"error":{"access_token":"[REDACTED]","metadata":{"message":"Bearer [REDACTED]"},"normal_key":"safe_value","vendor.detail":"Bearer [REDACTED]"},"sequence_number":3}"#
        );
    }

    // TestResponsesStreamErrorTextPreservesTokenCountersAndLargeInts
    #[test]
    fn error_text_preserves_token_counters_and_large_ints() {
        let out = text(
            400,
            r#"{"error":{"input_tokens":42,"token_limit":8192,"request_id":9007199254740993,"access_token":"secret123"}}"#,
        );
        assert_eq!(
            out,
            r#"{"error":{"access_token":"[REDACTED]","input_tokens":42,"request_id":9007199254740993,"token_limit":8192}}"#
        );
    }

    #[test]
    fn error_text_redacts_escaped_keys() {
        let out = text(
            502,
            r#"{"error":{"message":"oops","api\u005fkey":"SECRET","pass\u0077ord":"SECRET"}}"#,
        );
        assert_eq!(
            out,
            r#"{"error":{"api_key":"[REDACTED]","message":"oops","password":"[REDACTED]"}}"#
        );
    }

    /// An object 2 levels deep, then `arrays` nested arrays around one with
    /// a password.
    fn nested(arrays: usize) -> String {
        format!(
            r#"{{"error":{{"message":"oops","extra":{}{{"password":"SECRET"}}{}}}}}"#,
            "[".repeat(arrays),
            "]".repeat(arrays)
        )
    }

    #[test]
    fn error_text_fails_closed_on_objects_serde_cannot_read() {
        let review = r#"{"error":{"message":"oops","api\u005fkey":"SECRET","note":"\ud800"}}"#;
        let lone = r#"{"error":{"message":"oops","password":"SECRET","note":"\udc00x"}}"#;
        let deep = nested(126);
        for raw in [review, lone, deep.as_str()] {
            assert!(go::json_valid(raw.as_bytes()), "{raw}");
            assert!(serde_json::from_str::<Value>(raw).is_err(), "{raw}");
            assert_eq!(text(502, raw), "Bad Gateway", "{raw}");
            assert_eq!(text(429, raw), "Too Many Requests", "{raw}");
            let safe = sanitize_error(ErrorMessage::new(503, raw));
            assert_eq!(safe.status, 503);
            assert_eq!(safe.text, "Service Unavailable");
            let fallback = sanitize_error(ErrorMessage::new(200, raw));
            assert_eq!(fallback.status, 500);
            assert_eq!(fallback.text, "Internal Server Error");
        }
        // Nesting serde_json can read is still redacted field by field.
        assert_eq!(
            text(502, &nested(100)),
            format!(
                r#"{{"error":{{"extra":{}{{"password":"[REDACTED]"}}{},"message":"oops"}}}}"#,
                "[".repeat(100),
                "]".repeat(100)
            )
        );
    }

    #[test]
    fn error_text_redacts_arrays_by_field() {
        assert_eq!(
            text(502, r#"[{"password":"SECRET","message":"token=abc"},1]"#),
            r#"[{"message":"token=[REDACTED]","password":"[REDACTED]"},1]"#
        );
        let deep = format!(
            r#"[{}{{"password":"SECRET"}}{}]"#,
            "[".repeat(130),
            "]".repeat(130)
        );
        assert!(go::json_valid(deep.as_bytes()));
        assert_eq!(text(502, &deep), "Bad Gateway");
    }

    #[test]
    fn error_text_handles_other_shapes() {
        assert_eq!(text(502, ""), "Bad Gateway");
        assert_eq!(text(502, "  token=abc  "), "token=[REDACTED]");
        assert_eq!(text(502, "null"), "{}");
        assert_eq!(text(502, r#"["token=abc"]"#), r#"["token=[REDACTED]"]"#);
        assert_eq!(
            text(
                502,
                r#"{"response":{"error":{"b":1,"a":"x"}},"sequence_number":"s"}"#
            ),
            r#"{"error":{"a":"x","b":1},"sequence_number":"s"}"#
        );
        assert_eq!(
            text(502, r#"{"z":{"secret":"s","y":[1.50]},"a":null}"#),
            r#"{"a":null,"z":{"secret":"[REDACTED]","y":[1.50]}}"#
        );
        let long = text(502, &"é".repeat(3000));
        assert_eq!(long.chars().count(), MESSAGE_LIMIT + 1);
        assert!(long.ends_with("é…"));
    }

    // TestSanitizeResponsesStreamErrorMessageNormalizesSuccessStatus
    #[test]
    fn sanitize_normalizes_success_status() {
        let safe = sanitize_error(ErrorMessage::new(200, "upstream failed"));
        assert_eq!(safe.status, 500);
        assert_eq!(safe.text, "upstream failed");
        assert_eq!(
            sanitize_error(ErrorMessage::new(0, "")).text,
            "Internal Server Error"
        );
        assert_eq!(sanitize_error(ErrorMessage::new(429, "x")).status, 429);
    }

    // TestBuildOpenAIResponsesStreamErrorChunk
    #[test]
    fn builds_error_chunk() {
        assert_eq!(
            error_chunk(500, "unexpected EOF", 0),
            r#"{"type":"error","error":{"code":"internal_server_error","message":"unexpected EOF","param":null,"type":"server_error"},"sequence_number":0}"#
        );
    }

    // TestBuildOpenAIResponsesStreamErrorChunkExtractsHTTPErrorBody
    #[test]
    fn error_chunk_extracts_http_error_body() {
        let chunk = parse(&error_chunk(
            500,
            r#"{"error":{"message":"oops","type":"server_error","code":"internal_server_error"}}"#,
            0,
        ));
        assert_eq!(chunk["type"], "error");
        assert_eq!(chunk["error"]["code"], "internal_server_error");
        assert_eq!(chunk["error"]["message"], "oops");
        assert_eq!(chunk["error"]["type"], "server_error");
    }

    // TestBuildOpenAIResponsesStreamErrorChunkPreservesNestedError
    #[test]
    fn error_chunk_preserves_nested_error() {
        let chunk = error_chunk(
            400,
            r#"{"error":{"type":"invalid_request","code":"cyber_policy","message":"This content was flagged for possible cybersecurity risk.","param":null}}"#,
            2,
        );
        assert_eq!(
            chunk,
            r#"{"type":"error","error":{"code":"cyber_policy","message":"This content was flagged for possible cybersecurity risk.","param":null,"type":"invalid_request"},"sequence_number":2}"#
        );
    }

    // TestBuildOpenAIResponsesStreamErrorChunkPreservesCustomAndEmptyFields
    #[test]
    fn error_chunk_preserves_custom_and_empty_fields() {
        assert_eq!(
            error_chunk(400, r#"{"error":{}}"#, 0),
            r#"{"type":"error","error":{},"sequence_number":0}"#
        );
        let chunk = parse(&error_chunk(
            400,
            r#"{"error":{"type":"custom_type","code":"custom_code","custom_key":"custom_val","is_flag":true,"count":42}}"#,
            5,
        ));
        assert_eq!(chunk["sequence_number"], 5);
        assert_eq!(chunk["error"]["custom_key"], "custom_val");
        assert_eq!(chunk["error"]["is_flag"], true);
        assert_eq!(chunk["error"]["count"], 42);
    }

    // TestBuildOpenAIResponsesStreamErrorChunkPrioritizesPayloadSequenceNumber
    #[test]
    fn error_chunk_prioritizes_payload_sequence_number() {
        let text = r#"{"error":{"type":"invalid_request","code":"blocked"},"sequence_number":7}"#;
        assert_eq!(parse(&error_chunk(400, text, 2))["sequence_number"], 7);
        let negative = r#"{"error":{},"sequence_number":-4}"#;
        assert_eq!(parse(&error_chunk(400, negative, 2))["sequence_number"], -4);
        let fraction = r#"{"error":{},"sequence_number":7.0}"#;
        assert_eq!(parse(&error_chunk(400, fraction, -3))["sequence_number"], 0);
    }

    // TestBuildOpenAIResponsesStreamFailedChunkPreservesNestedError
    #[test]
    fn failed_chunk_preserves_nested_error() {
        assert_eq!(
            failed_chunk(
                400,
                r#"{"error":{"type":"invalid_request","code":"cyber_policy","message":"blocked","param":null}}"#,
                0,
            ),
            r#"{"type":"response.failed","sequence_number":0,"response":{"status":"failed","error":{"code":"cyber_policy","message":"blocked","param":null,"type":"invalid_request"}}}"#
        );
    }

    // TestBuildOpenAIResponsesStreamFailedChunkPrioritizesPayloadSequenceNumber
    #[test]
    fn failed_chunk_prioritizes_payload_sequence_number() {
        let text = r#"{"error":{"type":"invalid_request","code":"cyber_policy","message":"blocked"},"sequence_number":7}"#;
        assert_eq!(parse(&failed_chunk(400, text, 2))["sequence_number"], 7);
    }

    // TestBuildOpenAIResponsesStreamErrorChunkPreservesLargeIntPrecision
    #[test]
    fn chunks_preserve_large_int_precision() {
        let text = r#"{"error":{"type":"invalid_request","code":"blocked","request_id":9007199254740993}}"#;
        for chunk in [error_chunk(400, text, 0), failed_chunk(400, text, 0)] {
            assert!(chunk.contains("9007199254740993"), "{chunk}");
        }
    }

    // TestBuildOpenAIResponsesStreamErrorChunkRequestTimeoutIsServerError
    #[test]
    fn request_timeout_is_a_server_error() {
        let text = "stream disconnected before completion";
        let chunk = parse(&error_chunk(408, text, 0));
        assert_eq!(chunk["error"]["code"], "request_timeout");
        assert_eq!(chunk["error"]["type"], "server_error");
        let failed = parse(&failed_chunk(408, text, 0));
        assert_eq!(failed["response"]["error"]["code"], "request_timeout");
        assert_eq!(failed["response"]["error"]["type"], "server_error");
    }

    #[test]
    fn error_detail_reads_the_payload() {
        let chunk = error_chunk(
            401,
            r#"{"message":" denied ","code":{"b":[1,null],"a":"x"},"type":"auth","param":{"z":1,"y":2}}"#,
            1,
        );
        assert_eq!(
            chunk,
            r#"{"type":"error","error":{"code":"map[a:x b:[1 <nil>]]","message":"denied","param":{"y":2,"z":1},"type":"auth"},"sequence_number":1}"#
        );
        assert_eq!(
            parse(&error_chunk(404, r#"{"code":"  ","type":"error"}"#, 0))["error"],
            json!({"code": "", "message": r#"{"code":"  ","type":"error"}"#, "param": null, "type": "invalid_request_error"})
        );
        assert_eq!(
            parse(&error_chunk(429, r#"{"code":429}"#, 0))["error"]["code"],
            "429"
        );
        assert_eq!(
            parse(&error_chunk(0, "", 0))["error"],
            json!({"code": "internal_server_error", "message": "Internal Server Error", "param": null, "type": "server_error"})
        );
        assert_eq!(
            parse(&error_chunk(302, "moved", 0))["error"]["code"],
            "unknown_error"
        );
    }
}
