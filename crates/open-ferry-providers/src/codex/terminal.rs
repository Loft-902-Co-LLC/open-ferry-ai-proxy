// Ported from CLIProxyAPI internal/runtime/executor/codex_executor_terminal.go,
// statusErr in openai_compat_executor.go, normalizeCodexWebsocketCompletion in
// codex_websockets_errors.go and helps/codex_terminal_incomplete.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Codex's failures as errors: an HTTP error status, a stream's terminal
//! failure event (`error`, `response.failed`), an empty
//! `response.incomplete`, and a stream that ends without a terminal event.
//! Also the `response.output_item.done` items that fill in a completed
//! response's `output`.
//!
//! Deviations from upstream:
//! - Upstream's errors say whether they are scoped to the credential (a
//!   usage limit) or to the request (a stream that broke off). [`ExecError`]
//!   has no field for that, so [`StatusError`] keeps it and the conversion
//!   drops it.
//! - Model-level cooling, a config option, isn't ported, so a usage limit is
//!   always scoped to the credential.
//! - Bodies built from a stream event are written by `serde_json`, whose
//!   escapes and spacing may differ from sjson's.
//! - The stream-bootstrap helpers (overload probing while buffering) aren't
//!   ported, as bootstrap buffering isn't.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use open_ferry_core::exec::ExecError;
use serde_json::{Value, json};

use super::gjson::{eq_fold_trim, exists, get, int_at, int_of, set, str_at, str_of};

/// The error for a stream that ended before its terminal event.
pub(crate) const INCOMPLETE_STREAM_MESSAGE: &str =
    "stream error: stream disconnected before completion: stream closed before response.completed";

/// The error for a `response.incomplete` with no output at all.
pub(crate) const EMPTY_INCOMPLETE_STREAM_MESSAGE: &str =
    "stream error: upstream terminated with incomplete empty response (0 tokens)";

/// The error for a response whose `apply_patch` call couldn't be translated.
pub(crate) const APPLY_PATCH_ERROR_MESSAGE: &str =
    "Invalid apply_patch tool arguments received from upstream.";

/// An error with an HTTP status (upstream's `statusErr`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StatusError {
    /// The HTTP status.
    pub(crate) status: u16,
    /// The body, or empty.
    pub(crate) message: String,
    /// When to try again, from a usage limit's reset.
    pub(crate) retry_after: Option<Duration>,
    /// Whether it's the credential's fault, so others may still work.
    pub(crate) credential_scoped: bool,
    /// Whether it's this request's fault, not the credential's.
    pub(crate) request_scoped: bool,
}

impl StatusError {
    pub(crate) fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            retry_after: None,
            credential_scoped: false,
            request_scoped: false,
        }
    }

    /// The error's text, as upstream's `Error()` gives it.
    pub(crate) fn text(&self) -> String {
        if self.message.is_empty() {
            format!("status {}", self.status)
        } else {
            self.message.clone()
        }
    }
}

impl From<StatusError> for ExecError {
    fn from(error: StatusError) -> Self {
        let mut exec = ExecError::upstream(error.status, error.text());
        exec.retry_after = error.retry_after;
        exec
    }
}

/// A stream that ended before its terminal event (status 408).
pub(crate) fn incomplete_stream_error() -> StatusError {
    StatusError {
        request_scoped: true,
        ..StatusError::new(408, INCOMPLETE_STREAM_MESSAGE)
    }
}

/// A `response.incomplete` with no output at all (status 502).
pub(crate) fn empty_incomplete_stream_error() -> StatusError {
    StatusError {
        request_scoped: true,
        ..StatusError::new(502, EMPTY_INCOMPLETE_STREAM_MESSAGE)
    }
}

/// Go's `http.StatusText`, where it differs from the `http` crate's.
fn status_text(status: u16) -> &'static str {
    match status {
        413 => "Request Entity Too Large",
        414 => "Request URI Too Long",
        416 => "Requested Range Not Satisfiable",
        425 => "Too Early",
        _ => http::StatusCode::from_u16(status)
            .ok()
            .and_then(|status| status.canonical_reason())
            .unwrap_or(""),
    }
}

fn parse(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or(Value::Null)
}

fn lower_trim(text: &str) -> String {
    open_ferry_translate::go::to_lower(text.trim())
}

/// The error for Codex's error status and body (`newCodexStatusErr`). A
/// usage limit or a model at capacity becomes 429, and known failures get a
/// classified body.
pub(crate) fn status_error(status: u16, body: &[u8]) -> StatusError {
    status_error_at(status, body, SystemTime::now())
}

pub(crate) fn status_error_at(status: u16, body: &[u8], now: SystemTime) -> StatusError {
    let parsed = parse(body);
    let usage_limit = !body.is_empty() && is_usage_limit(&parsed);
    let status = if usage_limit || is_model_capacity(body, &parsed) {
        429
    } else {
        status
    };
    let (message, parsed) = match classify(status, body, &parsed) {
        Some(classified) => {
            let parsed = parse(classified.as_bytes());
            (classified, parsed)
        }
        None => (String::from_utf8_lossy(body).into_owned(), parsed),
    };
    StatusError {
        retry_after: parse_retry_after(status, &message, &parsed, now),
        credential_scoped: usage_limit,
        ..StatusError::new(status, message)
    }
}

/// A body with the failure's code and type, when it is one Codex clients
/// handle (`classifyCodexStatusError`).
fn classify(status: u16, body: &[u8], parsed: &Value) -> Option<String> {
    let (code, error_type) = classification(status, body, parsed)?;
    let mut message = str_at(parsed, "error.message");
    if message.is_empty() {
        message = str_at(parsed, "message");
    }
    if message.is_empty() {
        message = String::from_utf8_lossy(body).trim().to_owned();
    }
    if message.is_empty() {
        message = status_text(status).to_owned();
    }
    Some(json!({"error": {"message": message, "type": error_type, "code": code}}).to_string())
}

/// The code and type of a failure Codex clients handle
/// (`codexStatusErrorClassification`).
fn classification(
    status: u16,
    body: &[u8],
    parsed: &Value,
) -> Option<(&'static str, &'static str)> {
    let mut message = lower_trim(&str_at(parsed, "error.message"));
    if message.is_empty() {
        message = lower_trim(&str_at(parsed, "message"));
    }
    let lower = lower_trim(&String::from_utf8_lossy(body));
    let code = lower_trim(&str_at(parsed, "error.code"));
    let error_type = lower_trim(&str_at(parsed, "error.type"));
    let invalid_request = error_type.is_empty() || error_type == "invalid_request_error";
    let message_says_too_long = [
        "context length",
        "context_length",
        "maximum context",
        "too many tokens",
    ]
    .iter()
    .any(|needle| message.contains(needle));

    if status == 413
        || code == "context_length_exceeded"
        || code == "context_too_large"
        || (invalid_request && message_says_too_long)
    {
        Some(("context_too_large", "invalid_request_error"))
    } else if lower.contains("invalid signature in thinking block")
        || lower.contains("invalid_encrypted_content")
    {
        Some(("thinking_signature_invalid", "invalid_request_error"))
    } else if code == "previous_response_not_found"
        || lower.contains("previous_response_not_found")
        || (lower.contains("previous_response_id") && lower.contains("not found"))
    {
        Some(("previous_response_not_found", "invalid_request_error"))
    } else if status == 401
        || error_type == "authentication_error"
        || code == "invalid_api_key"
        || lower.contains("invalid or expired token")
        || lower.contains("refresh_token_reused")
    {
        Some(("auth_unavailable", "authentication_error"))
    } else {
        None
    }
}

/// Whether the body says the model is at capacity
/// (`isCodexModelCapacityError`).
fn is_model_capacity(body: &[u8], parsed: &Value) -> bool {
    if body.is_empty() {
        return false;
    }
    let candidates = [
        str_at(parsed, "error.message"),
        str_at(parsed, "message"),
        String::from_utf8_lossy(body).into_owned(),
    ];
    candidates.iter().any(|candidate| {
        let lower = lower_trim(candidate);
        !lower.is_empty()
            && (lower.contains("model is at capacity")
                || lower.contains("model_at_capacity")
                || lower.contains("model_is_at_capacity")
                || (lower.contains("model") && lower.contains("at capacity")))
    })
}

/// Whether the body says the credential's usage quota ran out
/// (`isCodexUsageLimitError`). A per-minute rate limit doesn't count.
fn is_usage_limit(parsed: &Value) -> bool {
    ["error.type", "type"]
        .iter()
        .any(|path| eq_fold_trim(&str_at(parsed, path), "usage_limit_reached"))
}

/// How long until a usage limit resets, for a 429 (`parseCodexRetryAfter`).
fn parse_retry_after(status: u16, body: &str, parsed: &Value, now: SystemTime) -> Option<Duration> {
    if status != 429 || body.is_empty() {
        return None;
    }
    let quotas = [get(parsed, "error"), Some(parsed)];
    for quota in quotas.into_iter().flatten() {
        if !eq_fold_trim(&str_at(quota, "type"), "usage_limit_reached") {
            continue;
        }
        let resets_at = int_at(quota, "resets_at");
        if resets_at > 0 {
            let reset = UNIX_EPOCH + Duration::from_secs(resets_at.unsigned_abs());
            if let Ok(wait) = reset.duration_since(now)
                && !wait.is_zero()
            {
                return Some(wait);
            }
        }
        let resets_in = int_at(quota, "resets_in_seconds");
        if resets_in > 0 {
            return Some(Duration::from_secs(resets_in.unsigned_abs()));
        }
    }
    None
}

/// The error body of a terminal failure event, `error` or
/// `response.failed` (`codexTerminalFailureBody`).
fn terminal_failure_body(event: &Value) -> Option<Value> {
    let mut body = match str_at(event, "type").as_str() {
        "error" => terminal_error_body(event, "error").or_else(|| top_level_error_body(event)),
        "response.failed" => terminal_error_body(event, "response.error")
            .or_else(|| terminal_error_body(event, "error")),
        _ => return None,
    }
    .unwrap_or_else(
        || json!({"error": {"message": "upstream stream failed without error details"}}),
    );
    if let Some(sequence) = get(event, "sequence_number") {
        set(
            &mut body,
            "sequence_number",
            Value::from(int_of(Some(sequence))),
        );
    }
    Some(body)
}

/// `{"error": ...}` from the event's value at `path`, with a message
/// (`codexTerminalErrorBody`).
fn terminal_error_body(event: &Value, path: &str) -> Option<Value> {
    let error = get(event, path)?;
    let mut body = json!({"error": {}});
    if error.is_object() || error.is_array() {
        set(&mut body, "error", error.clone());
    } else {
        let message = str_of(Some(error));
        let message = message.trim();
        if !message.is_empty() {
            set(&mut body, "error.message", Value::from(message));
        }
    }
    let fallbacks = [
        str_at(event, "response.error.message"),
        str_at(&body, "error.code"),
        str_at(&body, "error.type"),
    ];
    for fallback in fallbacks {
        if !str_at(&body, "error.message").trim().is_empty() {
            break;
        }
        let fallback = fallback.trim();
        if !fallback.is_empty() {
            set(&mut body, "error.message", Value::from(fallback));
        }
    }
    Some(body)
}

/// `{"error": ...}` from an `error` event's top-level fields
/// (`codexTerminalTopLevelErrorBody`).
fn top_level_error_body(event: &Value) -> Option<Value> {
    let field = |key: &str| str_at(event, key).trim().to_owned();
    let (message, code, error_type, param) = (
        field("message"),
        field("code"),
        field("error_type"),
        field("param"),
    );
    if message.is_empty() && code.is_empty() && error_type.is_empty() && param.is_empty() {
        return None;
    }
    let mut body = json!({"error": {}});
    for (key, value) in [
        ("message", &message),
        ("code", &code),
        ("type", &error_type),
        ("param", &param),
    ] {
        if !value.is_empty() {
            set(
                &mut body,
                &format!("error.{key}"),
                Value::from(value.as_str()),
            );
        }
    }
    if message.is_empty() {
        let fallback = if code.is_empty() { &error_type } else { &code };
        if !fallback.is_empty() {
            set(&mut body, "error.message", Value::from(fallback.as_str()));
        }
    }
    Some(body)
}

/// Whether the failure is the context being too long
/// (`codexTerminalErrorIsContextLength`).
fn is_context_length(body: &Value) -> bool {
    let code = lower_trim(&str_at(body, "error.code"));
    let message = lower_trim(&str_at(body, "error.message"));
    code == "context_length_exceeded"
        || code == "context_too_large"
        || message.contains("context window")
        || message.contains("context length")
        || message.contains("too many tokens")
}

/// Whether a terminal failure is one that becomes a 400-based error
/// (`codexTerminalStreamErrShouldHandle`).
fn stream_error_should_handle(body: &Value, raw: &[u8]) -> bool {
    is_context_length(body)
        || is_usage_limit(body)
        || is_model_capacity(raw, body)
        || classification(400, raw, body)
            .is_some_and(|(code, _)| code == "thinking_signature_invalid")
}

/// The error for a terminal failure event that clients handle as a bad
/// request (`codexTerminalStreamErr`).
pub(crate) fn terminal_stream_error(event: &Value) -> Option<StatusError> {
    let body = terminal_failure_body(event)?;
    let raw = body.to_string();
    stream_error_should_handle(&body, raw.as_bytes()).then(|| status_error(400, raw.as_bytes()))
}

/// The error for any terminal failure event (`codexTerminalFailureErr`).
pub(crate) fn terminal_failure_error(event: &Value) -> Option<StatusError> {
    if let Some(error) = terminal_stream_error(event) {
        return Some(error);
    }
    let body = terminal_failure_body(event)?;
    let raw = body.to_string();
    Some(status_error(terminal_failure_status(&body), raw.as_bytes()))
}

/// The status for a terminal failure's body (`codexTerminalFailureStatus`).
fn terminal_failure_status(body: &Value) -> u16 {
    for path in ["error.status_code", "error.status"] {
        if let Ok(status) = u16::try_from(int_at(body, path))
            && (400..=599).contains(&status)
        {
            return status;
        }
    }
    let error_type = lower_trim(&str_at(body, "error.type"));
    let code = lower_trim(&str_at(body, "error.code"));
    match (error_type.as_str(), code.as_str()) {
        (_, "cyber_policy") => 400,
        ("not_found_error", _) | (_, "not_found" | "model_not_found") => 404,
        ("authentication_error", _) | (_, "invalid_api_key" | "unauthorized") => 401,
        ("permission_error", _) | (_, "forbidden" | "permission_denied") => 403,
        ("rate_limit_error", _) | (_, "rate_limit_exceeded") => 429,
        ("invalid_request_error" | "bad_request_error", _) => 400,
        _ => 502,
    }
}

/// Whether the event carries generated content
/// (`HasMeaningfulCodexOutputDelta`).
pub(crate) fn has_meaningful_output_delta(event: &Value) -> bool {
    match str_at(event, "type").as_str() {
        "response.output_text.delta"
        | "response.reasoning_text.delta"
        | "response.reasoning_summary_text.delta"
        | "response.function_call_arguments.delta" => {
            exists(event, "delta") && !str_at(event, "delta").trim().is_empty()
        }
        _ => false,
    }
}

/// Whether a `response.incomplete` came with no output and an explicit zero
/// output tokens (`IsCodexTerminalEmptyIncomplete`).
pub(crate) fn is_terminal_empty_incomplete(
    event: &Value,
    output_items: usize,
    saw_output_delta: bool,
) -> bool {
    if str_at(event, "type") != "response.incomplete" || saw_output_delta || output_items > 0 {
        return false;
    }
    if get(event, "response.output")
        .and_then(Value::as_array)
        .is_some_and(|output| !output.is_empty())
    {
        return false;
    }
    matches!(get(event, "response.usage.output_tokens"), Some(Value::Number(n)) if n.to_string().trim() == "0")
}

/// Turns a `response.done` into a `response.completed`
/// (`normalizeCodexWebsocketCompletion`). Returns whether it changed.
pub(crate) fn normalize_completion(event: &mut Value) -> bool {
    if str_at(event, "type").trim() == "response.done" {
        return set(event, "type", Value::from("response.completed"));
    }
    false
}

/// The items of `response.output_item.done` events, to fill in a completed
/// response whose `output` came empty.
#[derive(Debug, Default)]
pub(crate) struct OutputItems {
    by_index: BTreeMap<i64, Value>,
    fallback: Vec<Value>,
}

impl OutputItems {
    /// How many items were kept.
    pub(crate) fn len(&self) -> usize {
        self.by_index.len() + self.fallback.len()
    }

    /// Keeps the event's item (`collectCodexOutputItemDone`).
    pub(crate) fn collect(&mut self, event: &Value) {
        let Some(item) = get(event, "item").filter(|item| item.is_object() || item.is_array())
        else {
            return;
        };
        match get(event, "output_index") {
            Some(index) => {
                self.by_index.insert(int_of(Some(index)), item.clone());
            }
            None => self.fallback.push(item.clone()),
        }
    }

    /// Fills in the completed event's `response.output` from the items, or
    /// the IDs its items lack (`patchCodexCompletedOutput`). Returns whether
    /// it changed.
    pub(crate) fn patch(&self, event: &mut Value) -> bool {
        let output_len = get(event, "response.output")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if output_len > 0 {
            return self.hydrate_ids(event, output_len);
        }
        if self.len() == 0 {
            return false;
        }
        let items: Vec<Value> = self
            .by_index
            .values()
            .chain(self.fallback.iter())
            .cloned()
            .collect();
        set(event, "response.output", Value::Array(items))
    }

    /// `hydrateCodexCompletedOutputItemIDs`.
    fn hydrate_ids(&self, event: &mut Value, output_len: usize) -> bool {
        let mut changed = false;
        for index in 0..output_len {
            let path = format!("response.output.{index}.id");
            let has_id = match get(event, &path) {
                None | Some(Value::Null) => false,
                Some(Value::String(id)) => !id.trim().is_empty(),
                Some(_) => true,
            };
            if has_id {
                continue;
            }
            let Some(completed) = i64::try_from(index)
                .ok()
                .and_then(|index| self.by_index.get(&index))
            else {
                continue;
            };
            let Some(Value::String(id)) = get(completed, "id") else {
                continue;
            };
            if id.trim().is_empty() {
                continue;
            }
            changed |= set(event, &path, Value::String(id.clone()));
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quota() -> &'static str {
        r#"{"type":"usage_limit_reached","message":"You've hit your usage limit.","resets_in_seconds":3600}"#
    }

    // TestCodexQuotaErrorCredentialScope, without the WebSocket paths.
    #[test]
    fn quota_errors_are_scoped_to_the_credential() {
        let quota = quota();
        let http_cases = [
            (format!(r#"{{"error":{quota}}}"#), true),
            (quota.to_owned(), true),
            (r#"{"error":{"type":"usage_limit_reached"}}"#.to_owned(), true),
            (
                r#"{"error":{"message":"Selected model is at capacity. Please try a different model."}}"#.to_owned(),
                false,
            ),
            (
                r#"{"error":{"type":"rate_limit_error","code":"rate_limit_exceeded"}}"#.to_owned(),
                false,
            ),
        ];
        for (body, want) in http_cases {
            let error = status_error(429, body.as_bytes());
            assert_eq!(error.credential_scoped, want, "{body}");
            if want && body.contains("resets_in_seconds") {
                assert_eq!(error.retry_after, Some(Duration::from_secs(3600)), "{body}");
            }
        }
        let terminal_cases = [
            format!(r#"{{"type":"error","error":{quota}}}"#),
            format!(r#"{{"type":"response.failed","response":{{"error":{quota}}}}}"#),
        ];
        for event in terminal_cases {
            let error = terminal_stream_error(&parse(event.as_bytes())).expect("recognized");
            assert!(error.credential_scoped, "{event}");
            assert_eq!(
                error.retry_after,
                Some(Duration::from_secs(3600)),
                "{event}"
            );
        }
    }

    #[test]
    fn retry_after_quota_layouts() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let cases = [
            (
                r#"{"type":"usage_limit_reached","resets_in_seconds":3600}"#,
                Some(3600),
            ),
            (
                r#"{"type":"usage_limit_reached","resets_at":1700000300,"resets_in_seconds":1}"#,
                Some(300),
            ),
            (
                r#"{"type":"usage_limit_reached","resets_at":1699999940,"resets_in_seconds":77}"#,
                Some(77),
            ),
            (
                r#"{"type":" USAGE_LIMIT_REACHED ","resets_in_seconds":30}"#,
                Some(30),
            ),
            (r#"{"type":"usage_limit_reached"}"#, None),
            (
                r#"{"type":"usage_limit_reached","resets_at":1699999940}"#,
                None,
            ),
            (
                r#"{"type":"usage_limit_reached","resets_at":0,"resets_in_seconds":-1}"#,
                None,
            ),
            (
                r#"{"type":"rate_limit_error","resets_in_seconds":30}"#,
                None,
            ),
        ];
        for (body, want) in cases {
            for layout in [body.to_owned(), format!(r#"{{"error":{body}}}"#)] {
                let got = parse_retry_after(429, &layout, &parse(layout.as_bytes()), now);
                assert_eq!(got, want.map(Duration::from_secs), "{layout}");
            }
        }
    }

    #[test]
    fn retry_after_needs_429_and_a_usage_limit() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let body = r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":123}}"#;
        assert_eq!(
            parse_retry_after(429, body, &parse(body.as_bytes()), now),
            Some(Duration::from_secs(123))
        );
        let body = r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":30}}"#;
        assert_eq!(
            parse_retry_after(400, body, &parse(body.as_bytes()), now),
            None
        );
        let body = r#"{"error":{"type":"server_error","resets_in_seconds":30}}"#;
        assert_eq!(
            parse_retry_after(429, body, &parse(body.as_bytes()), now),
            None
        );
        // Through the status error, with the clock injected.
        let body = br#"{"error":{"type":"usage_limit_reached","resets_at":1700000300,"resets_in_seconds":1}}"#;
        assert_eq!(
            status_error_at(429, body, now).retry_after,
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn capacity_is_a_rate_limit_without_retry_after() {
        let body = br#"{"error":{"message":"Selected model is at capacity. Please try a different model."}}"#;
        let error = status_error(400, body);
        assert_eq!(error.status, 429);
        assert_eq!(error.retry_after, None);
        assert!(!error.credential_scoped);
    }

    #[test]
    fn usage_limit_is_a_rate_limit_with_retry_after() {
        let body = br#"{"error":{"type":"usage_limit_reached","message":"You've hit your usage limit.","resets_in_seconds":120}}"#;
        let error = status_error(400, body);
        assert_eq!(error.status, 429);
        assert_eq!(error.retry_after, Some(Duration::from_secs(120)));
    }

    #[test]
    fn detects_usage_limits() {
        assert!(is_usage_limit(&parse(
            br#"{"error":{"type":"usage_limit_reached","resets_in_seconds":30}}"#
        )));
        assert!(is_usage_limit(&parse(br#"{"type":"usage_limit_reached"}"#)));
        assert!(!is_usage_limit(&parse(
            br#"{"error":{"type":"rate_limit_error","code":"rate_limit_exceeded"}}"#
        )));
        assert!(!is_usage_limit(&parse(b"")));
    }

    #[test]
    fn classifies_known_failures() {
        let cases = [
            (
                413,
                r#"{"error":{"message":"context length exceeded","type":"invalid_request_error","code":"context_length_exceeded"}}"#,
                "invalid_request_error",
                "context_too_large",
            ),
            (
                400,
                r#"{"error":{"message":"Invalid signature in thinking block","type":"invalid_request_error","code":"invalid_request_error"}}"#,
                "invalid_request_error",
                "thinking_signature_invalid",
            ),
            (
                400,
                r#"{"error":{"message":"No response found for previous_response_id resp_123","type":"invalid_request_error","code":"previous_response_not_found"}}"#,
                "invalid_request_error",
                "previous_response_not_found",
            ),
            (
                401,
                r#"{"error":{"message":"invalid or expired token","type":"authentication_error","code":"invalid_api_key"}}"#,
                "authentication_error",
                "auth_unavailable",
            ),
        ];
        for (status, body, want_type, want_code) in cases {
            let error = status_error(status, body.as_bytes());
            assert_eq!(error.status, status);
            let got = parse(error.message.as_bytes());
            assert_eq!(str_at(&got, "error.type"), want_type, "{body}");
            assert_eq!(str_at(&got, "error.code"), want_code, "{body}");
        }
    }

    #[test]
    fn keeps_unclassified_bodies() {
        let body = br#"{"error":{"message":"documentation mentions too many tokens, but this is a billing configuration failure","type":"server_error","code":"billing_config_error"}}"#;
        let error = status_error(502, body);
        assert_eq!(error.status, 502);
        assert_eq!(error.text().as_bytes(), body);
        assert_eq!(status_error(500, b"").text(), "status 500");
        // An empty classified body falls back to the status text.
        assert_eq!(
            status_error(401, b"").message,
            r#"{"error":{"message":"Unauthorized","type":"authentication_error","code":"auth_unavailable"}}"#
        );
    }

    #[test]
    fn terminal_failure_bodies_and_statuses() {
        let event = parse(br#"{"type":"response.failed","sequence_number":3,"response":{"error":{"code":"model_not_found"}}}"#);
        let error = terminal_failure_error(&event).expect("failure");
        assert_eq!(error.status, 404);
        assert_eq!(
            error.message,
            r#"{"error":{"code":"model_not_found","message":"model_not_found"},"sequence_number":3}"#
        );

        let event = parse(
            br#"{"type":"error","message":"slow down","code":"rate_limit_exceeded","param":"x"}"#,
        );
        let error = terminal_failure_error(&event).expect("failure");
        assert_eq!(error.status, 429);
        assert_eq!(
            error.message,
            r#"{"error":{"message":"slow down","code":"rate_limit_exceeded","param":"x"}}"#
        );

        let event = parse(br#"{"type":"error"}"#);
        let error = terminal_failure_error(&event).expect("failure");
        assert_eq!(error.status, 502);
        assert_eq!(
            error.message,
            r#"{"error":{"message":"upstream stream failed without error details"}}"#
        );

        let event = parse(br#"{"type":"response.failed","response":{"error":{"message":"boom","status_code":503}}}"#);
        assert_eq!(terminal_failure_error(&event).expect("failure").status, 503);

        let event = parse(br#"{"type":"error","error":{"type":"invalid_request_error","message":"This model's maximum context length is 1000 tokens."}}"#);
        let error = terminal_stream_error(&event).expect("context length is handled");
        assert_eq!(error.status, 400);
        assert_eq!(
            str_at(&parse(error.message.as_bytes()), "error.code"),
            "context_too_large"
        );

        // "context window" is handled as a bad request, but not classified.
        let event = parse(br#"{"type":"error","error":{"type":"invalid_request_error","message":"Your input exceeds the context window of this model."}}"#);
        let error = terminal_stream_error(&event).expect("context window is handled");
        assert_eq!(error.status, 400);
        assert_eq!(str_at(&parse(error.message.as_bytes()), "error.code"), "");

        let event = parse(br#"{"type":"error","error":{"type":"server_error","message":"oops"}}"#);
        assert!(terminal_stream_error(&event).is_none());
        assert!(terminal_failure_error(&parse(br#"{"type":"response.completed"}"#)).is_none());

        let event = parse(br#"{"type":"error","error":{"code":"cyber_policy","message":"no"}}"#);
        assert_eq!(terminal_failure_error(&event).expect("failure").status, 400);
    }

    // TestHasMeaningfulCodexOutputDelta and TestIsCodexTerminalEmptyIncomplete.
    #[test]
    fn meaningful_deltas() {
        let cases = [
            (
                r#"{"type":"response.output_text.delta","delta":"hi"}"#,
                true,
            ),
            (
                r#"{"type":"response.output_text.delta","delta":"   "}"#,
                false,
            ),
            (r#"{"type":"response.output_text.delta"}"#, false),
            (
                r#"{"type":"response.reasoning_text.delta","delta":"x"}"#,
                true,
            ),
            (
                r#"{"type":"response.reasoning_summary_text.delta","delta":"x"}"#,
                true,
            ),
            (
                r#"{"type":"response.function_call_arguments.delta","delta":"{"}"#,
                true,
            ),
            (r#"{"type":"response.created","delta":"x"}"#, false),
        ];
        for (event, want) in cases {
            assert_eq!(
                has_meaningful_output_delta(&parse(event.as_bytes())),
                want,
                "{event}"
            );
        }
    }

    #[test]
    fn terminal_empty_incomplete() {
        let empty = r#"{"type":"response.incomplete","response":{"output":[],"usage":{"output_tokens":0}}}"#;
        assert!(is_terminal_empty_incomplete(
            &parse(empty.as_bytes()),
            0,
            false
        ));
        assert!(!is_terminal_empty_incomplete(
            &parse(empty.as_bytes()),
            1,
            false
        ));
        assert!(!is_terminal_empty_incomplete(
            &parse(empty.as_bytes()),
            0,
            true
        ));
        let cases = [
            r#"{"type":"response.completed","response":{"output":[],"usage":{"output_tokens":0}}}"#,
            r#"{"type":"response.incomplete","response":{"output":[{"type":"message"}],"usage":{"output_tokens":0}}}"#,
            r#"{"type":"response.incomplete","response":{"output":[],"usage":{"output_tokens":0.0}}}"#,
            r#"{"type":"response.incomplete","response":{"output":[],"usage":{"output_tokens":"0"}}}"#,
            r#"{"type":"response.incomplete","response":{"output":[],"usage":{"output_tokens":null}}}"#,
            r#"{"type":"response.incomplete","response":{"output":[],"usage":{}}}"#,
            r#"{"type":"response.incomplete","response":{"output":[],"usage":{"output_tokens":5}}}"#,
        ];
        for event in cases {
            assert!(
                !is_terminal_empty_incomplete(&parse(event.as_bytes()), 0, false),
                "{event}"
            );
        }
    }

    #[test]
    fn patches_completed_output() {
        let mut items = OutputItems::default();
        items.collect(&parse(
            br#"{"type":"response.output_item.done","output_index":1,"item":{"id":"b"}}"#,
        ));
        items.collect(&parse(
            br#"{"type":"response.output_item.done","item":{"id":"c"}}"#,
        ));
        items.collect(&parse(
            br#"{"type":"response.output_item.done","output_index":0,"item":{"id":"a"}}"#,
        ));
        items.collect(&parse(
            br#"{"type":"response.output_item.done","output_index":2,"item":"text"}"#,
        ));
        assert_eq!(items.len(), 3);

        let mut event = parse(br#"{"type":"response.completed","response":{"output":[]}}"#);
        assert!(items.patch(&mut event));
        assert_eq!(
            event.to_string(),
            r#"{"type":"response.completed","response":{"output":[{"id":"a"},{"id":"b"},{"id":"c"}]}}"#
        );

        let mut event =
            parse(br#"{"type":"response.completed","response":{"output":[{"type":"message","id":" "},{"id":"keep"},{"type":"x"}]}}"#);
        assert!(items.patch(&mut event));
        assert_eq!(
            event.to_string(),
            r#"{"type":"response.completed","response":{"output":[{"type":"message","id":"a"},{"id":"keep"},{"type":"x"}]}}"#
        );

        let mut event = parse(br#"{"type":"response.completed","response":{}}"#);
        assert!(!OutputItems::default().patch(&mut event));

        let mut done = parse(br#"{"type":"response.done","response":{}}"#);
        assert!(normalize_completion(&mut done));
        assert_eq!(str_at(&done, "type"), "response.completed");
    }

    #[test]
    fn converts_to_exec_error() {
        let error: ExecError = StatusError {
            retry_after: Some(Duration::from_secs(5)),
            ..StatusError::new(429, "slow")
        }
        .into();
        assert_eq!(error.status, 429);
        assert_eq!(error.message, "slow");
        assert_eq!(error.retry_after, Some(Duration::from_secs(5)));
        assert_eq!(
            ExecError::from(StatusError::new(500, "")).message,
            "status 500"
        );
        assert!(incomplete_stream_error().request_scoped);
        assert_eq!(incomplete_stream_error().status, 408);
        assert_eq!(empty_incomplete_stream_error().status, 502);
    }

    // TestCodexTerminalFailureErrClassifiesStatus.
    #[test]
    fn terminal_failure_statuses() {
        let cases = [
            (
                r#"{"type":"error","error":{"type":"invalid_request_error","code":"invalid_value","message":"Invalid input."}}"#,
                400,
            ),
            (
                r#"{"type":"error","error":{"type":"invalid_request","code":"cyber_policy","message":"This content was flagged for possible cybersecurity risk."}}"#,
                400,
            ),
            (
                r#"{"type":"response.failed","response":{"error":{"type":"authentication_error","code":"invalid_api_key","message":"Invalid token."}}}"#,
                401,
            ),
            (
                r#"{"type":"error","error":{"type":"rate_limit_error","code":"rate_limit_exceeded","message":"Rate limit reached."}}"#,
                429,
            ),
            (
                r#"{"type":"response.failed","response":{"error":{"type":"upstream_error","code":"unknown","message":"Upstream failed."}}}"#,
                502,
            ),
            (
                r#"{"type":"error","error":{"type":"service_unavailable_error","code":"server_is_overloaded","message":"Our servers are currently overloaded. Please try again later."}}"#,
                502,
            ),
            (
                r#"{"type":"error","error":{"type":"invalid_request_error","code":"model_not_found","message":"The model gpt-5.5 does not exist or you do not have access to it."}}"#,
                404,
            ),
        ];
        for (event, want) in cases {
            let error = terminal_failure_error(&parse(event.as_bytes())).expect("handled");
            assert_eq!(error.status, want, "{event}");
        }
    }

    // TestCodexTerminalStreamContextLengthErrFromResponseFailed,
    // ...FromTopLevelError, ...IgnoresOtherTerminalErrors and
    // TestCodexTerminalStreamErrIgnoresRateLimitTerminalErrors.
    #[test]
    fn context_length_terminal_errors() {
        let event = parse(br#"{"type":"response.failed","response":{"id":"resp_1","status":"failed","error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model. Please adjust your input and try again."}}}"#);
        let error = terminal_stream_error(&event).expect("context length");
        assert_eq!(error.status, 400);
        let body = parse(error.message.as_bytes());
        assert_eq!(str_at(&body, "error.type"), "invalid_request_error");
        assert_eq!(str_at(&body, "error.code"), "context_too_large");

        let event = parse(br#"{"type":"error","code":"context_length_exceeded","message":"Your input exceeds the context window of this model. Please adjust your input and try again.","sequence_number":2}"#);
        let error = terminal_stream_error(&event).expect("top-level context length");
        assert_eq!(error.status, 400);
        let body = parse(error.message.as_bytes());
        assert_eq!(str_at(&body, "error.type"), "invalid_request_error");
        assert_eq!(str_at(&body, "error.code"), "context_too_large");
        assert!(
            error
                .message
                .contains("Your input exceeds the context window"),
            "{}",
            error.message
        );

        let event = parse(br#"{"type":"error","error":{"type":"rate_limit_error","code":"rate_limit_exceeded","message":"Rate limit reached."}}"#);
        assert!(terminal_stream_error(&event).is_none());
    }

    // TestCodexTerminalStreamErrHandlesUsageLimitErrorEvent and
    // TestCodexTerminalStreamErrHandlesUsageLimitResponseFailed.
    #[test]
    fn usage_limit_terminal_errors() {
        let event = parse(br#"{"type":"error","error":{"type":"usage_limit_reached","message":"You've hit your usage limit.","resets_in_seconds":300}}"#);
        let error = terminal_stream_error(&event).expect("usage limit event");
        assert_eq!(error.status, 429);
        assert_eq!(error.retry_after, Some(Duration::from_secs(300)));

        let event = parse(br#"{"type":"response.failed","response":{"error":{"type":"usage_limit_reached","message":"usage limit reached","resets_in_seconds":60}}}"#);
        let error = terminal_stream_error(&event).expect("usage limit response.failed");
        assert_eq!(error.status, 429);
        assert!(error.retry_after.is_some());
    }
}
