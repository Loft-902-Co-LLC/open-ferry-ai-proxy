// Ported from CLIProxyAPI internal/interfaces/error_message.go,
// sdk/api/handlers/handlers.go (BuildErrorResponseBodyWithError),
// sdk/api/handlers/handlers_errors.go, executionErrorMessage in
// sdk/api/handlers/handlers_execution.go and the error helpers in
// sdk/api/handlers/claude/code_handlers.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Errors as clients see them: [`ErrorMessage`], and the OpenAI and Claude
//! bodies and responses built from one.

use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use open_ferry_core::exec::ExecError;
use open_ferry_core::manager::clienterror::is_claude_thread_not_found;
use open_ferry_translate::go;
use serde_json::{Map, Value, json};

use crate::headers::is_reserved_response_header;
use crate::json::{compact, marshal_html};
use crate::status::status_text;

/// `Content-Type` for errors the handlers build themselves (gin's `c.JSON`).
pub(crate) const JSON_UTF8: &str = "application/json; charset=utf-8";

/// A failed call, as the handlers pass it around (upstream's
/// `interfaces.ErrorMessage`).
#[derive(Clone, Debug)]
pub struct ErrorMessage {
    /// The HTTP status, or 0 for none, which clients see as 500.
    pub status: u16,
    /// The error's text, or empty when there is none, which clients see as
    /// the status text.
    pub text: String,
    /// Whether the provider rejected the credential for good.
    pub terminal_auth: bool,
    /// The `Retry-After` value upstream trusts the error to set.
    pub retry_after: Option<HeaderValue>,
    /// The provider's headers that came with the error, sent on when
    /// `passthrough-headers` is on.
    pub addon: HeaderMap,
    /// The call's error, when the message came from one.
    pub source: Option<ExecError>,
}

impl ErrorMessage {
    /// A message the HTTP layer makes itself.
    pub fn new(status: u16, text: impl Into<String>) -> Self {
        Self {
            status,
            text: text.into(),
            terminal_auth: false,
            retry_after: None,
            addon: HeaderMap::new(),
            source: None,
        }
    }

    /// A call's error as a message (upstream's `executionErrorMessage`).
    pub fn from_exec(error: ExecError) -> Self {
        let status = match error.http_status() {
            0 => 500,
            status => status,
        };
        Self {
            status,
            text: error.to_string(),
            terminal_auth: error.terminal_auth,
            retry_after: error.retry_after_header(),
            addon: error.headers.clone(),
            source: Some(error),
        }
    }

    /// The status to answer with: [`ErrorMessage::status`], or 500.
    pub fn http_status(&self) -> u16 {
        if self.status > 0 { self.status } else { 500 }
    }

    /// The text an error response carries: the trimmed text, or the status
    /// text when that's empty.
    fn response_text(&self, status: u16) -> String {
        match self.text.trim() {
            "" => status_text(status).to_owned(),
            text => text.to_owned(),
        }
    }

    /// The headers an error response carries besides `Content-Type`: the
    /// trusted `Retry-After`, then, with `passthrough`, the provider's
    /// headers, each replacing any value already set.
    fn response_headers(&self, passthrough: bool) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(value) = &self.retry_after {
            headers.append(header::RETRY_AFTER, value.clone());
        }
        if passthrough {
            for name in self.addon.keys() {
                if is_reserved_response_header(name) {
                    continue;
                }
                headers.remove(name);
                for value in self.addon.get_all(name) {
                    headers.append(name.clone(), value.clone());
                }
            }
        }
        headers
    }
}

/// An OpenAI error body (upstream's `BuildErrorResponseBodyWithError`). A
/// body that is already JSON is kept, compacted onto one line as a stream's
/// `data:` line needs it, unless the credential was rejected for good.
pub(crate) fn openai_body(status: u16, err_text: &str, terminal_auth: bool) -> String {
    let status = if status == 0 { 500 } else { status };
    let err_text = if err_text.trim().is_empty() {
        status_text(status)
    } else {
        err_text
    };
    let trimmed = err_text.trim();

    if terminal_auth {
        let mut message = err_text.to_owned();
        if go::json_valid(trimmed.as_bytes())
            && let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(trimmed)
        {
            if let Some(found) = non_empty_string(&parsed, "message") {
                message = found.to_owned();
            } else if let Some(Value::Object(error)) = parsed.get("error")
                && let Some(found) = non_empty_string(error, "message")
            {
                message = found.to_owned();
            }
        }
        return marshal_html(&json!({"error": {
            "message": message,
            "type": "authentication_error",
            "code": "upstream_authentication_required",
            "retryable": false,
        }}));
    }

    if !trimmed.is_empty() && go::json_valid(trimmed.as_bytes()) {
        return compact(trimmed);
    }

    let (kind, code) = match status {
        401 => ("authentication_error", "invalid_api_key"),
        403 => ("permission_error", "insufficient_quota"),
        429 => ("rate_limit_error", "rate_limit_exceeded"),
        404 => ("invalid_request_error", "model_not_found"),
        408 => ("server_error", "request_timeout"),
        500.. => ("server_error", "internal_server_error"),
        _ => ("invalid_request_error", ""),
    };
    let mut error = Map::new();
    error.insert("message".into(), err_text.into());
    error.insert("type".into(), kind.into());
    if !code.is_empty() {
        error.insert("code".into(), code.into());
    }
    marshal_html(&json!({ "error": Value::Object(error) }))
}

/// The string at `key` in `object`, if it is a non-empty string.
fn non_empty_string<'v>(object: &'v Map<String, Value>, key: &str) -> Option<&'v str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// The OpenAI error body for `message`, as a non-streaming response carries
/// it.
pub(crate) fn openai_error_body(message: &ErrorMessage) -> Bytes {
    let status = message.http_status();
    let text = message.response_text(status);
    Bytes::from(openai_body(status, &text, message.terminal_auth))
}

/// The response for an OpenAI endpoint's error (upstream's
/// `WriteErrorResponse`).
pub(crate) fn openai_error_response(message: &ErrorMessage, passthrough: bool) -> Response {
    let headers = message.response_headers(passthrough);
    error_response(
        message.http_status(),
        headers,
        openai_error_body(message),
        "application/json",
    )
}

/// The Claude error body for `message` (upstream's `toClaudeError`). The
/// 404 Anthropic answers a stale `previous_message_id` with is marked with
/// the `thread_not_found` error code, telling the client to replay the
/// whole conversation.
pub(crate) fn claude_error_json(message: &ErrorMessage) -> String {
    let status = message.http_status();
    let text = match message.text.trim() {
        "" => status_text(status),
        text => text,
    };
    let (kind, message) = claude_error_detail(status, text);
    let mut error = json!({"type": kind, "message": message});
    if is_claude_thread_not_found(status, text)
        && let Value::Object(error) = &mut error
    {
        error.insert("details".into(), json!({"error_code": "thread_not_found"}));
    }
    marshal_html(&json!({"type": "error", "error": error}))
}

/// The type and message of a Claude error (upstream's
/// `claudeErrorDetailFromText`): from the text when it is a JSON error,
/// otherwise the type for the status.
fn claude_error_detail(status: u16, err_text: &str) -> (String, String) {
    let mut message = match err_text.trim() {
        "" => status_text(status).to_owned(),
        text => text.to_owned(),
    };
    let mut kind = claude_error_type(status).to_owned();
    if go::json_valid(message.as_bytes())
        && let Ok(Value::Object(payload)) = serde_json::from_str::<Value>(&message)
    {
        let trimmed = |object: &Map<String, Value>, key: &str| {
            object
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        if let Some(Value::Object(error)) = payload.get("error") {
            if let Some(found) = trimmed(error, "type") {
                kind = found;
            }
            if let Some(found) = trimmed(error, "message").or_else(|| trimmed(error, "code")) {
                message = found;
            }
        } else {
            if let Some(found) = trimmed(&payload, "type").filter(|t| t != "error") {
                kind = found;
            }
            if let Some(found) = trimmed(&payload, "message") {
                message = found;
            }
        }
    }
    (kind, message)
}

/// The Claude error type for a status.
fn claude_error_type(status: u16) -> &'static str {
    match status {
        401 => "authentication_error",
        402 => "billing_error",
        403 => "permission_error",
        404 => "not_found_error",
        408 | 504 => "timeout_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        500.. => "api_error",
        _ => "invalid_request_error",
    }
}

/// The response for a Claude endpoint's error (the Claude handler's
/// `WriteErrorResponse`).
pub(crate) fn claude_error_response(message: &ErrorMessage, passthrough: bool) -> Response {
    let headers = message.response_headers(passthrough);
    error_response(
        message.http_status(),
        headers,
        Bytes::from(claude_error_json(message)),
        "application/json",
    )
}

/// A 400 for a request the handler couldn't read (upstream's `c.JSON` with
/// `Invalid request: <err>`), or a 413 for one that was too big.
pub(crate) fn invalid_request(status: u16, err: &str) -> Response {
    local_error(
        status,
        &format!("Invalid request: {err}"),
        "invalid_request_error",
    )
}

/// An error the handler answers itself, in upstream's `ErrorResponse` shape
/// with no code (gin's `c.JSON`, which escapes `<`, `>` and `&` as
/// `json.Marshal` does).
pub(crate) fn local_error(status: u16, message: &str, kind: &str) -> Response {
    let body = marshal_html(&json!({"error": {"message": message, "type": kind}}));
    error_response(status, HeaderMap::new(), Bytes::from(body), JSON_UTF8)
}

/// A JSON response with `status`, `headers` and `body`, and `content_type`
/// over any `Content-Type` in `headers`.
pub(crate) fn error_response(
    status: u16,
    headers: HeaderMap,
    body: Bytes,
    content_type: &'static str,
) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() =
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    *response.headers_mut() = headers;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use open_ferry_core::exec::ErrorKind;
    use std::time::Duration;

    #[test]
    fn openai_bodies_match_upstream() {
        assert_eq!(
            openai_body(429, "slow down", false),
            r#"{"error":{"message":"slow down","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#
        );
        assert_eq!(
            openai_body(400, "  ", false),
            r#"{"error":{"message":"Bad Request","type":"invalid_request_error"}}"#
        );
        assert_eq!(
            openai_body(0, "", false),
            r#"{"error":{"message":"Internal Server Error","type":"server_error","code":"internal_server_error"}}"#
        );
        // JSON is kept, trimmed.
        assert_eq!(openai_body(400, " {\"a\":1} ", false), "{\"a\":1}");
        // Text keeps its spaces.
        assert_eq!(
            openai_body(418, " x ", false),
            r#"{"error":{"message":" x ","type":"invalid_request_error"}}"#
        );
    }

    /// Ported from upstream's handlers_error_response_test.go
    /// (TestBuildErrorResponseBody_CompactsPrettyPrintedJSON): a JSON body
    /// is compacted onto one line, as a stream's `data:` line needs it.
    #[test]
    fn json_bodies_are_compacted() {
        let pretty = "{\n  \"error\": {\n    \"code\": 500,\n    \"message\": \"Internal error encountered.\",\n    \"status\": \"INTERNAL\"\n  }\n}";
        let body = openai_body(500, pretty, false);
        assert!(!body.contains('\n'), "{body}");
        assert_eq!(
            body,
            r#"{"error":{"code":500,"message":"Internal error encountered.","status":"INTERNAL"}}"#
        );
        // Not upstream's: white space in strings stays, escapes and all, and
        // nothing is escaped that wasn't.
        assert_eq!(
            openai_body(
                400,
                "[ \"a b\\\" \\n\" ,\t{ \"<\" : \"\u{2028}\" } ]",
                false
            ),
            "[\"a b\\\" \\n\",{\"<\":\"\u{2028}\"}]"
        );
    }

    #[test]
    fn terminal_auth_bodies_take_the_inner_message() {
        let auth = |text| openai_body(401, text, true);
        let expect = |m: &str| {
            format!(
                r#"{{"error":{{"message":"{m}","type":"authentication_error","code":"upstream_authentication_required","retryable":false}}}}"#
            )
        };
        assert_eq!(auth(r#"{"message":"top"}"#), expect("top"));
        assert_eq!(auth(r#"{"error":{"message":"inner"}}"#), expect("inner"));
        assert_eq!(
            auth(r#"{"message":"","error":{"message":"inner"}}"#),
            expect("inner")
        );
        assert_eq!(auth("plain"), expect("plain"));
        assert_eq!(auth("[1]"), expect("[1]"));
    }

    /// Not upstream's: the bodies built here escape `<`, `>`, `&`, U+2028
    /// and U+2029 as Go's `json.Marshal` (and so gin's `c.JSON`) does.
    #[tokio::test]
    async fn bodies_escape_as_go_does() {
        // Go trims U+2028 from the ends of a Claude message, as Rust does.
        let text = "a <b> & c \u{2028} d";
        let want = r#"a \u003cb\u003e \u0026 c \u2028 d"#;
        assert_eq!(
            openai_body(400, text, false),
            format!(r#"{{"error":{{"message":"{want}","type":"invalid_request_error"}}}}"#)
        );
        assert!(openai_body(401, text, true).contains(want));
        let claude = claude_error_json(&ErrorMessage::new(400, text));
        assert!(claude.contains(want), "{claude}");
        let response = local_error(400, text, "invalid_request_error");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            body,
            format!(r#"{{"error":{{"message":"{want}","type":"invalid_request_error"}}}}"#)
        );
    }

    #[test]
    fn claude_errors_match_upstream() {
        let message = ErrorMessage::new(529, "");
        // Go has no text for 529.
        assert_eq!(
            claude_error_json(&message),
            r#"{"type":"error","error":{"type":"overloaded_error","message":""}}"#
        );
        let upstream = ErrorMessage::new(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":" bad "}}"#,
        );
        assert_eq!(
            claude_error_json(&upstream),
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#
        );
        let code_only = ErrorMessage::new(429, r#"{"error":{"code":"rate_limited"}}"#);
        assert_eq!(
            claude_error_json(&code_only),
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"rate_limited"}}"#
        );
        let flat = ErrorMessage::new(503, r#"{"type":"error","message":"down"}"#);
        assert_eq!(
            claude_error_json(&flat),
            r#"{"type":"error","error":{"type":"api_error","message":"down"}}"#
        );
        let none = ErrorMessage::new(0, "");
        assert_eq!(
            claude_error_json(&none),
            r#"{"type":"error","error":{"type":"api_error","message":"Internal Server Error"}}"#
        );
    }

    // TestClaudeErrorTypeFromStatus.
    #[test]
    fn claude_error_types_follow_the_status() {
        for (status, want) in [
            (400, "invalid_request_error"),
            (408, "timeout_error"),
            (429, "rate_limit_error"),
            (500, "api_error"),
            (504, "timeout_error"),
        ] {
            assert_eq!(claude_error_type(status), want, "{status}");
        }
    }

    const MISSING_THREAD: &str = r#"{"type":"error","error":{"type":"not_found_error","message":"No thread state was found for the requested previous_message_id. Replay the full conversation with thread create to start a new Thread."}}"#;

    fn error_code(body: &str) -> Value {
        let body: Value = serde_json::from_str(body).unwrap();
        body["error"]["details"]["error_code"].clone()
    }

    // TestClaudeErrorMarksMissingThreadForClientReplay.
    #[test]
    fn claude_errors_mark_a_missing_thread_for_replay() {
        let body = claude_error_json(&ErrorMessage::new(404, MISSING_THREAD));
        assert_eq!(error_code(&body), "thread_not_found", "{body}");
        assert_eq!(
            body,
            r#"{"type":"error","error":{"type":"not_found_error","message":"No thread state was found for the requested previous_message_id. Replay the full conversation with thread create to start a new Thread.","details":{"error_code":"thread_not_found"}}}"#
        );
        // Not upstream's: the same body with another status, or another
        // 404, isn't marked.
        let body = claude_error_json(&ErrorMessage::new(500, MISSING_THREAD));
        assert!(!body.contains("details"), "{body}");
        let body = claude_error_json(&ErrorMessage::new(
            404,
            r#"{"error":{"type":"not_found_error","message":"Not Found"}}"#,
        ));
        assert!(!body.contains("details"), "{body}");
    }

    // TestClaudeErrorMarksWrappedMissingThreadForClientReplay. A call's
    // error carries the provider's body as its text, so there is no
    // separate response body to prefer.
    #[test]
    fn claude_errors_mark_a_missing_thread_from_a_call() {
        let error = ExecError::upstream(
            404,
            r#"{"type":"error","error":{"type":"not_found_error","message":"No thread state was found for the requested previous_message_id."}}"#,
        );
        let body = claude_error_json(&ErrorMessage::from_exec(error));
        assert_eq!(error_code(&body), "thread_not_found", "{body}");
    }

    // v8.0.20's TestClaudeErrorMarksPlainTextMissingThreadForClientReplay.
    // Its TestClaudeErrorMarksStructuredExecutorMissingThreadForClientReplay
    // is the case above: an error whose text is the structured body.
    #[test]
    fn claude_errors_mark_a_plain_text_missing_thread_for_replay() {
        let text = "No thread state was found for the requested previous_message_id. Replay the full conversation with thread create to start a new Thread.";
        let body = claude_error_json(&ErrorMessage::from_exec(ExecError::upstream(404, text)));
        assert_eq!(error_code(&body), "thread_not_found", "{body}");
        let body = claude_error_json(&ErrorMessage::new(404, "thread gone"));
        assert!(!body.contains("details"), "{body}");
    }

    // TestWriteClaudeDirectErrorMarksMissingThreadForClientReplay.
    #[tokio::test]
    async fn claude_error_responses_mark_a_missing_thread_for_replay() {
        use http_body_util::BodyExt;

        let response = claude_error_response(&ErrorMessage::new(404, MISSING_THREAD), false);
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(error_code(&body), "thread_not_found", "{body}");
    }

    #[test]
    fn execution_errors_become_messages() {
        let message = ErrorMessage::from_exec(ExecError::auth_unavailable(Duration::from_secs(3)));
        assert_eq!(message.status, 503);
        assert_eq!(message.text, "auth_unavailable: no auth available");
        assert_eq!(message.retry_after.as_ref().unwrap(), "3");

        let message = ErrorMessage::from_exec(ExecError::auth_not_found());
        assert_eq!(message.status, 500);
        let message = ErrorMessage::from_exec(ExecError::new(ErrorKind::Canceled, "gone"));
        assert_eq!(message.status, 499);
    }

    #[test]
    fn error_headers_keep_reserved_ones_out() {
        let mut error = ExecError::upstream(429, "slow");
        error
            .headers
            .insert("x-request-id", HeaderValue::from_static("abc"));
        error.headers.insert(
            "access-control-allow-origin",
            HeaderValue::from_static("evil"),
        );
        let message = ErrorMessage::from_exec(error);
        let response = openai_error_response(&message, true);
        assert_eq!(response.status(), 429);
        assert_eq!(response.headers()["x-request-id"], "abc");
        assert!(
            !response
                .headers()
                .contains_key("access-control-allow-origin")
        );
        assert_eq!(response.headers()["content-type"], "application/json");
        let response = openai_error_response(&message, false);
        assert!(!response.headers().contains_key("x-request-id"));
    }
}
