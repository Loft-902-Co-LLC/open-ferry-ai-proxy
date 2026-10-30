// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_cooldown.go (the error
// classifiers) and the error codes in sdk/cliproxy/auth/errors.go (v8.0.42,
// MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! How the manager reads a failure: whose fault it is, whether it should
//! cool the credential down, and whether another credential could help.
//!
//! Upstream asks an error chain through `errors.As`; here an [`ErrView`] is
//! either an executor's [`ExecError`] or a recorded [`AuthError`], and
//! answers the same questions.
//!
//! Deviations from upstream:
//! - Connection failures are known by [`TransportFault`] where upstream
//!   matches Go's network error types; their text is still checked as
//!   upstream does. A [`TransportFault::Lifecycle`] fault counts as a
//!   WebSocket close (a lifecycle failure even with a status) and as an EOF
//!   (a transient one).
//! - Upstream's `isConfiguredTLSHandshakeTimeoutError` (the typed deadline
//!   of its configured TLS handshake bound) has no source here, as there is
//!   no such bound. A [`TransportFault::Transient`] fault, which only an
//!   executor sets, plays its part: it makes the error a transient transport
//!   failure even when its kind is a deadline, and never a lifecycle one.
//! - A closed connection is known by the executor's typed [`TransportFault`]
//!   as well as hyper's and Go's texts (see
//!   [`is_http2_closed_connection_message`]). Executors preserve unambiguous
//!   EOF and socket error types before flattening their messages.
//! - A body that fails after a success status is typed
//!   [`TransportFault::ResponseBody`]. It keeps the credential out of
//!   cooldown but, by its type alone, is not a transient failure and starts
//!   no round, because the provider may already have done the work. Upstream
//!   (Go) retries it, as `io.ErrUnexpectedEOF` is transient there; here only
//!   text that main already matched still does.
//! - The manager's own errors carry their cause as a summary, so the
//!   classifiers don't look through to it.

use std::time::Duration;

use serde_json::Value;

use super::clienterror;
use super::credential::is_zero;
use super::text::{equal_fold, go_lower, parse_suffix};
use crate::auth::{Auth, AuthError, Status};
use crate::exec::{ErrorKind, ExecError, Options, TransportFault};

/// Upstream's `ErrorCodeRequestScoped`.
pub(crate) const CODE_REQUEST_SCOPED: &str = "request_scoped";
/// Upstream's `ErrorCodeConnectionLifecycle`.
pub(crate) const CODE_CONNECTION_LIFECYCLE: &str = "connection_lifecycle";
/// Upstream's `ErrorCodeTransientTransport`.
pub(crate) const CODE_TRANSIENT_TRANSPORT: &str = "transient_transport";
/// Upstream's `ErrorCodeForceCooldown`.
pub(crate) const CODE_FORCE_COOLDOWN: &str = "force_cooldown";

/// An error as the classifiers see it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ErrView<'a> {
    /// An executor's error, or one of the manager's own.
    Exec(&'a ExecError),
    /// A failure recorded on a credential (upstream's `*Error`).
    Auth(&'a AuthError),
}

/// The `*Error` inside an error, as `errors.As` finds it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AuthParts<'a> {
    pub(crate) code: &'a str,
    pub(crate) message: &'a str,
    pub(crate) status: u16,
    pub(crate) retryable: bool,
}

impl AuthParts<'_> {
    /// The `*Error`'s own text, without any cause.
    pub(crate) fn text(&self) -> String {
        error_text(self.code, self.message)
    }

    pub(crate) fn to_auth_error(self) -> AuthError {
        AuthError {
            code: self.code.to_owned(),
            message: self.message.to_owned(),
            retryable: self.retryable,
            http_status: self.status,
        }
    }
}

/// `*Error.Error()`: the message, after the code when there is one.
pub(crate) fn error_text(code: &str, message: &str) -> String {
    if code.is_empty() {
        message.to_owned()
    } else {
        format!("{code}: {message}")
    }
}

/// `*Error.Error()` of a recorded failure.
pub(crate) fn auth_error_text(err: &AuthError) -> String {
    error_text(&err.code, &err.message)
}

impl<'a> ErrView<'a> {
    /// The error's status, or 0 (upstream's `statusCodeFromError`).
    pub(crate) fn status(self) -> u16 {
        match self {
            Self::Exec(err) => err.status,
            Self::Auth(err) => err.http_status,
        }
    }

    /// The error's text (upstream's `err.Error()`).
    pub(crate) fn text(self) -> String {
        match self {
            Self::Exec(err) => err.to_string(),
            Self::Auth(err) => auth_error_text(err),
        }
    }

    /// The `*Error` in the error, if it has one.
    pub(crate) fn auth_parts(self) -> Option<AuthParts<'a>> {
        match self {
            Self::Exec(err) => err.kind.code().map(|code| AuthParts {
                code,
                message: &err.message,
                status: err.status,
                retryable: err.kind == ErrorKind::EmptyStream
                    || (err.kind == ErrorKind::AuthUnavailable
                        && err.status == 503
                        && !err.terminal_auth),
            }),
            Self::Auth(err) => Some(AuthParts {
                code: &err.code,
                message: &err.message,
                status: err.http_status,
                retryable: err.retryable,
            }),
        }
    }

    fn kind(self) -> Option<ErrorKind> {
        match self {
            Self::Exec(err) => Some(err.kind),
            Self::Auth(_) => None,
        }
    }

    fn transport(self) -> Option<TransportFault> {
        match self {
            Self::Exec(err) => err.transport,
            Self::Auth(_) => None,
        }
    }

    fn is_context_error(self) -> bool {
        matches!(
            self.kind(),
            Some(ErrorKind::Canceled | ErrorKind::DeadlineExceeded)
        )
    }
}

/// Whether the failure is this request's alone (upstream's
/// `isRequestScopedError`).
pub(crate) fn is_request_scoped_error(err: ErrView<'_>) -> bool {
    match err {
        ErrView::Exec(err) => err.request_scoped,
        ErrView::Auth(err) => err.code == CODE_REQUEST_SCOPED,
    }
}

/// Whether the failure is the credential's as a whole (upstream's
/// `isCredentialScopedError`).
pub(crate) fn is_credential_scoped_error(err: ErrView<'_>) -> bool {
    matches!(err, ErrView::Exec(err) if err.credential_scoped)
}

/// The provider's own retry hint (upstream's `retryAfterFromError`). Only an
/// executor's error carries one; the manager's errors don't offer theirs.
pub(crate) fn retry_after_from_error(err: ErrView<'_>) -> Option<Duration> {
    match err {
        ErrView::Exec(err) if err.kind == ErrorKind::Upstream => err.effective_retry_after(),
        _ => None,
    }
}

/// The failure as recorded on a credential (upstream's
/// `resultErrorFromError`).
pub(crate) fn result_error_from_error(err: ErrView<'_>) -> AuthError {
    let mut result = match err.auth_parts() {
        Some(parts) => parts.to_auth_error(),
        None => AuthError {
            message: err.text(),
            ..AuthError::default()
        },
    };
    if result.http_status == 0 {
        result.http_status = err.status();
    }
    if let ErrView::Exec(error) = err
        && let Some(code) = error.failure_code()
    {
        result.code = code.into();
    } else if is_explicit_model_not_found_error(err, "") {
        if result.code.is_empty() || result.code == CODE_REQUEST_SCOPED {
            result.code = "model_not_found".into();
        }
    } else if is_request_scoped_error(err) || is_request_invalid_error(err) {
        result.code = CODE_REQUEST_SCOPED.into();
    } else if is_connection_lifecycle_error(err) {
        if result.code.is_empty() || result.code == CODE_CONNECTION_LIFECYCLE {
            result.code = CODE_CONNECTION_LIFECYCLE.into();
        }
    } else if is_transient_transport_error(err)
        && (result.code.is_empty() || result.code == CODE_TRANSIENT_TRANSPORT)
    {
        result.code = CODE_TRANSIENT_TRANSPORT.into();
    } else if err.transport() == Some(TransportFault::ResponseBody)
        && err.status() == 0
        && (result.code.is_empty() || result.code == CODE_CONNECTION_LIFECYCLE)
    {
        // Not transient (it must not start a round), but still not the
        // credential's fault, so it stays out of cooldown. Text main already
        // matched took one of the branches above and keeps its own code.
        result.code = CODE_CONNECTION_LIFECYCLE.into();
    }
    result
}

/// Whether a failure must not cool the credential down (upstream's
/// `shouldSkipCredentialCooldown`).
pub(crate) fn should_skip_credential_cooldown(err: Option<&AuthError>) -> bool {
    let Some(err) = err else {
        return false;
    };
    if err.code == CODE_FORCE_COOLDOWN {
        return false;
    }
    matches!(err.code.as_str(), "stream_truncated")
        || is_request_scoped_result_error(err)
        || is_connection_lifecycle_result_error(err)
        || is_transient_transport_result_error(err)
}

/// Whether the call or connection ended early, which isn't the credential's
/// fault (upstream's `isConnectionLifecycleError`).
pub(crate) fn is_connection_lifecycle_error(err: ErrView<'_>) -> bool {
    if err.transport() == Some(TransportFault::Lifecycle) {
        return true;
    }
    if err.status() != 0 {
        return false;
    }
    // An executor's own transport deadline, not the caller's.
    if err.transport() == Some(TransportFault::Transient) {
        return false;
    }
    if err.is_context_error() {
        return true;
    }
    is_connection_lifecycle_message(&err.text())
}

/// Upstream's `isConnectionLifecycleResultError`.
pub(crate) fn is_connection_lifecycle_result_error(err: &AuthError) -> bool {
    if err.code == CODE_CONNECTION_LIFECYCLE {
        return true;
    }
    if err.code == CODE_TRANSIENT_TRANSPORT {
        return false;
    }
    if err.http_status != 0 {
        return false;
    }
    is_connection_lifecycle_message(&err.message)
}

/// Upstream's `isConnectionLifecycleMessage`.
pub(crate) fn is_connection_lifecycle_message(message: &str) -> bool {
    let lower = go_lower(message.trim());
    if lower.is_empty() {
        return false;
    }
    if matches!(
        lower.as_str(),
        "context canceled" | "context deadline exceeded" | "eof" | "unexpected eof"
    ) {
        return true;
    }
    lower.contains("websocket: close 1000")
        || lower.contains("websocket: close 1001")
        || lower.contains("websocket: close 1006")
        || lower.contains("unexpected eof")
        || is_http2_closed_connection_message(&lower)
}

/// Whether a lowercased message says the HTTP/2 client connection was closed
/// under the call (upstream's `isHTTP2ClosedConnectionMessage`).
///
/// Upstream matches Go's `x/net/http2` texts: `http2: client connection force
/// closed via ClientConn.Close` and `http2: client conn is closed`. Our HTTP
/// client is hyper, which reports a call that met a dropped connection as a
/// canceled operation: `operation was canceled: connection closed`. That is
/// matched too, and hyper's `connection closed before message completed`,
/// the same call cut off after it was sent.
fn is_http2_closed_connection_message(lower: &str) -> bool {
    lower.contains("http2: client connection force closed via clientconn.close")
        || lower.contains("http2: client conn is closed")
        || (lower.contains("operation was canceled") && lower.contains("connection closed"))
        || lower.contains("connection closed before message completed")
}

/// Whether the connection failed before any answer, so another round may
/// work without cooling the credential (upstream's
/// `isTransientTransportError`).
pub(crate) fn is_transient_transport_error(err: ErrView<'_>) -> bool {
    if err.status() != 0 {
        return false;
    }
    if err.transport() == Some(TransportFault::Transient) {
        return true;
    }
    if err.is_context_error() {
        return false;
    }
    // Only a call that ended early counts by its type alone. A body that
    // failed after a success status ([`TransportFault::ResponseBody`]) is
    // judged by its text, as before the type existed: the provider may have
    // done the work, so a round that resends it needs words that main already
    // matched.
    if err.transport() == Some(TransportFault::Lifecycle) {
        return true;
    }
    is_transient_transport_message(&err.text())
}

/// Upstream's `isTransientTransportResultError`.
pub(crate) fn is_transient_transport_result_error(err: &AuthError) -> bool {
    if err.code == CODE_TRANSIENT_TRANSPORT {
        return true;
    }
    if err.http_status != 0 {
        return false;
    }
    // Once an error is reduced to a recorded message, its type is gone. The
    // transient code set by `result_error_from_error` is what vouches for a
    // handshake timeout; the same words in a bare message might come from an
    // external transport or the caller's own deadline.
    if go_lower(&err.message).contains("tls handshake timeout") {
        return false;
    }
    is_transient_transport_message(&err.message)
}

/// Upstream's `isTransientTransportMessage`.
pub(crate) fn is_transient_transport_message(message: &str) -> bool {
    let lower = go_lower(message.trim());
    if lower.is_empty() {
        return false;
    }
    [
        "tls: tls handshake",
        "tls handshake timeout",
        "wsarecv",
        "wsasend",
        "a connection attempt failed",
        "connection refused",
        "connection reset",
        "i/o timeout",
        "no such host",
        "server misbehaving",
        "network is unreachable",
        "no route to host",
        "broken pipe",
        "connection aborted",
        "use of closed network connection",
        "unexpected eof",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
        || is_http2_closed_connection_message(&lower)
}

/// Upstream's `isUnauthorizedError`.
pub(crate) fn is_unauthorized_error(err: ErrView<'_>) -> bool {
    if err.status() == 401 {
        return true;
    }
    let raw = go_lower(&err.text());
    raw.contains("status 401") || raw.contains("401 unauthorized")
}

/// Whether the credential's last failure was a 401 with no refresh or retry
/// pending: it is out of rotation until its tokens change or a forced
/// refresh works (upstream's `hasUnauthorizedAuthFailure` and
/// `HasUnauthorizedAuthFailure`).
pub fn has_unauthorized_auth_failure(auth: &Auth) -> bool {
    let Some(last) = &auth.last_error else {
        return false;
    };
    auth.unavailable
        && auth.status == Status::Error
        && is_zero(auth.next_refresh_after)
        && is_zero(auth.next_retry_after)
        && (last.http_status == 401 || equal_fold(&last.code, "unauthorized"))
}

/// Whether the credential was turned off after an `invalid_grant` (upstream's
/// `hasDisabledInvalidGrantFailure`).
pub(crate) fn has_disabled_invalid_grant_failure(auth: &Auth) -> bool {
    if !(auth.disabled || auth.status == Status::Disabled) {
        return false;
    }
    auth.last_error.as_ref().is_some_and(|last| {
        is_invalid_grant_result_error(last)
            || is_invalid_grant_message(&last.message)
            || is_invalid_grant_message(&last.code)
    })
}

/// A refresh failure as recorded on the credential (upstream's
/// `refreshErrorFromError`).
pub(crate) fn refresh_error_from_error(err: ErrView<'_>) -> AuthError {
    let mut status = err.status();
    if status == 0 && is_unauthorized_error(err) {
        status = 401;
    }
    let mut result = AuthError {
        message: err.text(),
        http_status: status,
        ..AuthError::default()
    };
    if status == 401 {
        result.code = "unauthorized".into();
        result.retryable = false;
    }
    result
}

/// Upstream's `isModelSupportErrorMessage`.
pub(crate) fn is_model_support_error_message(message: &str) -> bool {
    let lower = go_lower(message.trim());
    if lower.is_empty() {
        return false;
    }
    [
        "model_not_supported",
        "requested model is not supported",
        "requested model is unsupported",
        "requested model is unavailable",
        "model is not supported",
        "model not supported",
        "unsupported model",
        "model unavailable",
        "not available for your plan",
        "not available for your account",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
}

/// Whether the credential can't serve the model (upstream's
/// `isModelSupportError`).
pub(crate) fn is_model_support_error(err: ErrView<'_>) -> bool {
    if is_explicit_model_not_found_error(err, "") {
        return true;
    }
    if !matches!(err.status(), 400 | 422 | 404) {
        return false;
    }
    is_model_support_error_message(&err.text())
}

/// Upstream's `isModelSupportResultError`.
pub(crate) fn is_model_support_result_error(err: &AuthError) -> bool {
    if is_explicit_model_not_found_error(ErrView::Auth(err), "") {
        return true;
    }
    if !matches!(err.http_status, 400 | 422 | 404) {
        return false;
    }
    is_model_support_error_message(&err.message)
}

/// Upstream's `isInvalidGrantErrorMessage`.
pub(crate) fn is_invalid_grant_message(message: &str) -> bool {
    go_lower(message).contains("invalid_grant")
}

/// Whether the refresh token was rejected (upstream's
/// `isInvalidGrantError`).
pub(crate) fn is_invalid_grant_error(err: ErrView<'_>) -> bool {
    is_invalid_grant_message(&err.text()) && matches!(err.status(), 400 | 401 | 0)
}

/// Upstream's `isInvalidGrantResultError`.
pub(crate) fn is_invalid_grant_result_error(err: &AuthError) -> bool {
    (is_invalid_grant_message(&err.code) || is_invalid_grant_message(&err.message))
        && matches!(err.http_status, 400 | 401 | 0)
}

/// Upstream's `isCloudflareChallengeErrorMessage`.
pub(crate) fn is_cloudflare_challenge_message(message: &str) -> bool {
    let lower = go_lower(message.trim());
    lower.contains("challenge-platform")
        || lower.contains("cf-mitigated")
        || lower.contains("cloudflare challenge")
        || (lower.contains("just a moment") && lower.contains("cloudflare"))
}

/// Whether the provider's Cloudflare front served a challenge (upstream's
/// `isCloudflareChallengeError`). A 5xx is an origin failure instead.
pub(crate) fn is_cloudflare_challenge_error(err: ErrView<'_>) -> bool {
    if err.status() >= 500 {
        return false;
    }
    is_cloudflare_challenge_message(&err.text())
}

/// Upstream's `isCloudflareChallengeResultError`.
pub(crate) fn is_cloudflare_challenge_result_error(err: &AuthError) -> bool {
    if err.http_status >= 500 {
        return false;
    }
    is_cloudflare_challenge_message(&err.message)
}

/// Upstream's `isRequestScopedNotFoundResultError`.
pub(crate) fn is_request_scoped_not_found_result_error(err: &AuthError) -> bool {
    err.http_status == 404 && clienterror::is_item_not_persisted(&err.message)
}

/// Upstream's `isRequestScopedResultError`.
pub(crate) fn is_request_scoped_result_error(err: &AuthError) -> bool {
    if err.code == CODE_REQUEST_SCOPED || is_request_scoped_not_found_result_error(err) {
        return true;
    }
    is_request_invalid_error(ErrView::Auth(err))
}

/// Whether a token count's 404 is the endpoint missing rather than the
/// model (upstream's `isCountTokensEndpointNotFoundError`).
pub(crate) fn is_count_tokens_endpoint_not_found_error(
    err: ErrView<'_>,
    requested_model: &str,
) -> bool {
    if err.status() != 404 {
        return false;
    }
    let base = parse_suffix(requested_model).0;
    !is_explicit_model_not_found_error(err, base)
}

/// Upstream's `isResponsesCompactRequest`.
pub(crate) fn is_responses_compact_request(opts: &Options) -> bool {
    opts.alt == "responses/compact"
}

/// Upstream's `isResponsesCompactRequestFaultError`.
pub(crate) fn is_responses_compact_request_fault_error(opts: &Options, err: ErrView<'_>) -> bool {
    if !is_responses_compact_request(opts) {
        return false;
    }
    if is_credential_scoped_error(err)
        || is_cloudflare_challenge_error(err)
        || is_invalid_grant_error(err)
    {
        return false;
    }
    let status = err.status();
    if clienterror::is_request_fault(status, &err.text()) {
        return true;
    }
    matches!(status, 400 | 404 | 405 | 409 | 413 | 422 | 501)
}

/// Upstream's `isResponsesCompactAvailabilityNeutralError`.
pub(crate) fn is_responses_compact_availability_neutral_error(
    opts: &Options,
    err: ErrView<'_>,
    result_err: Option<&AuthError>,
) -> bool {
    if !is_responses_compact_request(opts) {
        return false;
    }
    if result_err.is_some_and(|r| r.code == CODE_FORCE_COOLDOWN) {
        return false;
    }
    if is_credential_scoped_error(err)
        || is_cloudflare_challenge_error(err)
        || is_invalid_grant_error(err)
    {
        return false;
    }
    if result_err.is_some_and(|r| {
        is_cloudflare_challenge_result_error(r) || is_invalid_grant_result_error(r)
    }) {
        return false;
    }
    let mut status = err.status();
    if status == 0
        && let Some(r) = result_err
    {
        status = r.http_status;
    }
    !matches!(status, 401 | 402 | 403 | 429)
}

/// Whether the provider said it has no such model (upstream's
/// `isExplicitModelNotFoundError`).
pub(crate) fn is_explicit_model_not_found_error(err: ErrView<'_>, requested_model: &str) -> bool {
    match err.auth_parts() {
        Some(parts) => {
            is_model_not_found_identifier(parts.code)
                || is_structured_model_not_found_error(parts.message, requested_model)
                || is_structured_model_not_found_error(&parts.text(), requested_model)
        }
        None => is_structured_model_not_found_error(&err.text(), requested_model),
    }
}

fn is_structured_model_not_found_error(message: &str, requested_model: &str) -> bool {
    match serde_json::from_str::<Value>(message.trim()) {
        Ok(payload) => contains_structured_model_not_found(&payload, requested_model),
        Err(_) => false,
    }
}

fn contains_structured_model_not_found(value: &Value, requested_model: &str) -> bool {
    match value {
        Value::Object(map) => {
            let mut not_found_type = false;
            let mut exact_model_reference = false;
            for (key, item) in map {
                if let Value::String(text) = item {
                    match go_lower(key.trim()).as_str() {
                        "code" => {
                            if is_model_not_found_identifier(text) {
                                return true;
                            }
                        }
                        "type" => {
                            if is_model_not_found_identifier(text) {
                                return true;
                            }
                            not_found_type = not_found_type || is_not_found_error_identifier(text);
                        }
                        "error" | "message" | "detail" | "error_description" | "title" => {
                            if is_explicit_model_not_found_message(text, requested_model) {
                                return true;
                            }
                            exact_model_reference = exact_model_reference
                                || is_exact_requested_model_reference(text, requested_model);
                        }
                        _ => {}
                    }
                }
                if matches!(item, Value::Object(_) | Value::Array(_))
                    && contains_structured_model_not_found(item, requested_model)
                {
                    return true;
                }
            }
            not_found_type && exact_model_reference
        }
        Value::Array(items) => items.iter().any(|item| {
            matches!(item, Value::String(text) if is_explicit_model_not_found_message(text, requested_model))
                || contains_structured_model_not_found(item, requested_model)
        }),
        _ => false,
    }
}

/// Upstream's `isModelNotFoundIdentifier`.
pub(crate) fn is_model_not_found_identifier(value: &str) -> bool {
    let lowered = go_lower(value.trim());
    let mut candidate = lowered.as_str();
    match candidate.rfind('#') {
        Some(fragment) if fragment + 1 < candidate.len() => {
            candidate = &candidate[fragment + 1..];
        }
        _ => {
            if let Some(query) = candidate.find('?') {
                candidate = &candidate[..query];
            }
            candidate = candidate.trim_end_matches('/');
            if let Some(separator) = candidate.rfind(['/', ':']) {
                candidate = &candidate[separator + 1..];
            }
        }
    }
    let normalized = candidate.replace(['-', ' '], "_");
    matches!(
        normalized.as_str(),
        "model_not_found"
            | "model_not_found_error"
            | "unknown_model"
            | "model_does_not_exist"
            | "model_not_exist"
    )
}

fn is_not_found_error_identifier(value: &str) -> bool {
    let normalized = go_lower(value.trim()).replace(['-', ' '], "_");
    normalized == "not_found" || normalized == "not_found_error"
}

const TRIM_SET: &[char] = &[' ', '.', '!', ';', '\t', '\r', '\n'];

/// `prefix`, matched as a whole word or before a colon.
fn strip_word_prefix<'a>(lower: &'a str, prefix: &str) -> Option<&'a str> {
    if lower == prefix {
        return Some("");
    }
    let rest = lower.strip_prefix(prefix)?;
    if rest.starts_with(' ') || rest.starts_with(':') {
        Some(rest)
    } else {
        None
    }
}

fn after_prefix(rest: &str) -> &str {
    let rest = rest.trim();
    rest.strip_prefix(':').unwrap_or(rest).trim()
}

/// Upstream's `isExplicitModelNotFoundMessage`.
pub(crate) fn is_explicit_model_not_found_message(message: &str, requested_model: &str) -> bool {
    let lowered = go_lower(message.trim());
    let lower = lowered.trim_matches(TRIM_SET);
    if lower.is_empty() {
        return false;
    }
    if lower.contains("in request") || lower.contains("in body") || lower.contains("request body") {
        return false;
    }
    let normalized = lower.replace('-', "_");
    if normalized.contains("model_not_found") || normalized.contains("unknown_model") {
        return true;
    }
    for prefix in ["no such model", "unknown model"] {
        let Some(rest) = strip_word_prefix(lower, prefix) else {
            continue;
        };
        let remainder = after_prefix(rest);
        if remainder.is_empty() {
            return true;
        }
        return matches!(
            trim_requested_model_reference(remainder, requested_model),
            Some(suffix) if suffix.is_empty()
        );
    }
    for prefix in [
        "the requested model",
        "requested model",
        "the model",
        "model",
    ] {
        let Some(rest) = strip_word_prefix(lower, prefix) else {
            continue;
        };
        let remainder = after_prefix(rest);
        if is_missing_model_phrase(remainder) {
            return true;
        }
        return matches!(
            trim_requested_model_reference(remainder, requested_model),
            Some(suffix) if is_missing_model_phrase(suffix)
        );
    }
    false
}

fn is_exact_requested_model_reference(message: &str, requested_model: &str) -> bool {
    let lowered = go_lower(message.trim());
    let lower = lowered.trim_matches(TRIM_SET);
    for prefix in [
        "the requested model",
        "requested model",
        "the model",
        "model",
    ] {
        let Some(rest) = strip_word_prefix(lower, prefix) else {
            continue;
        };
        let remainder = after_prefix(rest);
        return matches!(
            trim_requested_model_reference(remainder, requested_model),
            Some(suffix) if suffix.is_empty()
        );
    }
    false
}

/// What follows the requested model's name at the start of `value`, if it
/// starts with it, bare or quoted (upstream's
/// `trimRequestedModelReference`).
fn trim_requested_model_reference<'a>(value: &'a str, requested_model: &str) -> Option<&'a str> {
    let model = go_lower(requested_model.trim());
    if model.is_empty() {
        return None;
    }
    let candidates = [
        model.clone(),
        format!("'{model}'"),
        format!("\"{model}\""),
        format!("`{model}`"),
    ];
    for candidate in &candidates {
        if value == candidate {
            return Some("");
        }
        let Some(remainder) = value.strip_prefix(candidate.as_str()) else {
            continue;
        };
        if remainder.is_empty() || remainder.starts_with([' ', ':', ',']) {
            return Some(remainder.trim_start_matches([' ', ':', ',']));
        }
    }
    None
}

fn is_missing_model_phrase(value: &str) -> bool {
    matches!(
        value.trim_matches(TRIM_SET),
        "not found"
            | "was not found"
            | "could not be found"
            | "does not exist"
            | "doesn't exist"
            | "not exist"
            | "is unknown"
            | "does not exist or you do not have access to it"
    )
}

/// Whether the client's request is at fault, so no credential should rotate
/// or cool down for it (upstream's `isRequestInvalidError`). A model the
/// credential can't serve isn't the request's fault.
pub(crate) fn is_request_invalid_error(err: ErrView<'_>) -> bool {
    if matches!(err, ErrView::Exec(error) if error.stops_replay()) {
        return true;
    }

    if is_request_scoped_error(err) {
        return true;
    }
    if is_cloudflare_challenge_error(err)
        || is_invalid_grant_error(err)
        || is_model_support_error(err)
    {
        return false;
    }
    let status = err.status();
    if clienterror::is_request_fault(status, &err.text()) {
        return true;
    }
    if let Some(parts) = err.auth_parts()
        && !parts.message.is_empty()
        && clienterror::is_request_fault(status, parts.message)
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(status: u16, body: &str) -> ExecError {
        ExecError::upstream(status, body)
    }

    #[test]
    fn model_not_found_shapes() {
        let cases = [
            (r#"{"error":{"code":"model_not_found"}}"#, true),
            (
                r#"{"error":{"type":"https://x/errors#model-not-found"}}"#,
                true,
            ),
            (
                r#"{"error":{"type":"not_found_error","message":"model: gpt-5"}}"#,
                true,
            ),
            (
                r#"{"error":{"type":"not_found_error","message":"model: gpt-4"}}"#,
                false,
            ),
            (r#"{"detail":"The model `gpt-5` does not exist"}"#, true),
            (
                r#"{"message":"model gpt-5 in request body was not found"}"#,
                false,
            ),
            (r#"["unknown model"]"#, true),
            ("model not found", false),
        ];
        for (body, want) in cases {
            let err = upstream(404, body);
            assert_eq!(
                is_explicit_model_not_found_error(ErrView::Exec(&err), "gpt-5"),
                want,
                "{body}"
            );
        }
        assert!(is_model_not_found_identifier("urn:x:Model-Not-Found"));
        assert!(is_model_not_found_identifier("model does not exist"));
        assert!(!is_model_not_found_identifier("not_found"));
    }

    #[test]
    fn explicit_messages() {
        assert!(is_explicit_model_not_found_message("No such model", ""));
        assert!(is_explicit_model_not_found_message(
            "unknown model: 'gpt-5'",
            "gpt-5"
        ));
        assert!(!is_explicit_model_not_found_message(
            "unknown model: gpt-5 today",
            "gpt-5"
        ));
        assert!(is_explicit_model_not_found_message(
            "The model was not found.",
            ""
        ));
        assert!(is_explicit_model_not_found_message(
            "Model \"gpt-5\": not found",
            "gpt-5"
        ));
        assert!(!is_explicit_model_not_found_message(
            "models are fine",
            "gpt-5"
        ));
    }

    #[test]
    fn result_error_codes() {
        let err = upstream(400, r#"{"error":{"code":"invalid_value"}}"#);
        let result = result_error_from_error(ErrView::Exec(&err));
        assert_eq!(result.code, CODE_REQUEST_SCOPED);
        assert_eq!(result.http_status, 400);

        let err = upstream(404, r#"{"error":{"code":"model_not_found"}}"#);
        assert_eq!(
            result_error_from_error(ErrView::Exec(&err)).code,
            "model_not_found"
        );

        let err = ExecError::upstream(0, "read tcp: unexpected EOF");
        assert_eq!(
            result_error_from_error(ErrView::Exec(&err)).code,
            CODE_CONNECTION_LIFECYCLE
        );

        let err = ExecError::upstream(0, "dial").with_transport(TransportFault::Transient);
        assert_eq!(
            result_error_from_error(ErrView::Exec(&err)).code,
            CODE_TRANSIENT_TRANSPORT
        );

        let err = upstream(429, "slow down");
        let result = result_error_from_error(ErrView::Exec(&err));
        assert_eq!(result.code, "");
        assert_eq!(result.message, "slow down");
        assert!(!should_skip_credential_cooldown(Some(&result)));

        let err = ExecError::auth_unavailable(Duration::from_secs(3));
        let result = result_error_from_error(ErrView::Exec(&err));
        assert_eq!(result.code, "auth_unavailable");
        assert!(result.retryable);
    }

    #[test]
    fn request_invalid_and_model_support() {
        let model_support = upstream(400, "The requested model is not supported.");
        assert!(is_model_support_error(ErrView::Exec(&model_support)));
        assert!(!is_request_invalid_error(ErrView::Exec(&model_support)));
        let cloudflare = upstream(403, "<title>Just a moment...</title> cloudflare");
        assert!(is_cloudflare_challenge_error(ErrView::Exec(&cloudflare)));
        assert!(!is_request_invalid_error(ErrView::Exec(&cloudflare)));
        let grant = upstream(400, r#"{"error":"invalid_grant"}"#);
        assert!(is_invalid_grant_error(ErrView::Exec(&grant)));
        assert!(!is_request_invalid_error(ErrView::Exec(&grant)));
        let bad = upstream(422, "bad input");
        assert!(is_request_invalid_error(ErrView::Exec(&bad)));
        let recorded = AuthError {
            code: "x".into(),
            message: r#"{"error":{"type":"invalid_request_error"}}"#.into(),
            http_status: 500,
            ..AuthError::default()
        };
        assert!(is_request_invalid_error(ErrView::Auth(&recorded)));
    }

    #[test]
    fn transport_classes() {
        let refused = upstream(0, "dial tcp: connect: connection refused");
        assert!(is_transient_transport_error(ErrView::Exec(&refused)));
        assert!(!is_connection_lifecycle_error(ErrView::Exec(&refused)));
        let canceled = ExecError::canceled();
        assert!(is_connection_lifecycle_error(ErrView::Exec(&canceled)));
        assert!(!is_transient_transport_error(ErrView::Exec(&canceled)));
        let with_status = upstream(502, "connection reset");
        assert!(!is_transient_transport_error(ErrView::Exec(&with_status)));
        let lifecycle = upstream(0, "x").with_transport(TransportFault::Lifecycle);
        assert!(is_connection_lifecycle_error(ErrView::Exec(&lifecycle)));
        assert!(is_transient_transport_error(ErrView::Exec(&lifecycle)));
    }

    /// A body that failed after the provider sent a success status, as an
    /// executor reports it.
    fn body_failed(status: u16, message: &str) -> ExecError {
        ExecError::upstream(status, message).with_transport(TransportFault::ResponseBody)
    }

    /// Everything the manager decides about one failure: whether another
    /// round may start and what is recorded on the credential.
    #[derive(Debug, PartialEq)]
    struct Decisions {
        transient: bool,
        lifecycle: bool,
        retry_round: bool,
        skips_cooldown: bool,
        result_transient: bool,
        result_lifecycle: bool,
        code: String,
    }

    fn decisions(err: &ExecError) -> Decisions {
        let view = ErrView::Exec(err);
        let result = result_error_from_error(view);
        Decisions {
            transient: is_transient_transport_error(view),
            lifecycle: is_connection_lifecycle_error(view),
            retry_round: crate::manager::retry::is_request_retry_round_error(err),
            skips_cooldown: should_skip_credential_cooldown(Some(&result)),
            result_transient: is_transient_transport_result_error(&result),
            result_lifecycle: is_connection_lifecycle_result_error(&result),
            code: result.code,
        }
    }

    /// Text main already matched decides a body failure exactly as it decides
    /// the same text without the marker: the retry, the cooldown skip and the
    /// recorded code. Resets, aborts, broken pipes and EOFs are the kinds the
    /// executors promote.
    #[test]
    fn response_body_keeps_decisions_for_text_main_matched() {
        let transient = [
            "read tcp 127.0.0.1:1->127.0.0.1:2: connection reset by peer (os error 104)",
            "error decoding response body: connection reset",
            "wsarecv: an existing connection was forcibly closed by the remote host",
            "error reading a body from connection: connection aborted",
            "write tcp 127.0.0.1:1->127.0.0.1:2: broken pipe",
            "stream closed because of a broken pipe",
        ];
        let lifecycle_and_transient = [
            "unexpected EOF during chunk size line",
            "error decoding response body: unexpected EOF",
            "connection closed before message completed",
        ];
        let check = |text: &str, code: &str, retries: bool| {
            let plain = decisions(&upstream(0, text));
            assert_eq!(decisions(&body_failed(0, text)), plain, "{text}");
            assert_eq!(plain.code, code, "{text}");
            assert_eq!(plain.transient, retries, "{text}");
            assert_eq!(plain.retry_round, retries, "{text}");
            assert!(plain.skips_cooldown, "{text}");
        };
        for text in transient {
            check(text, CODE_TRANSIENT_TRANSPORT, true);
        }
        for text in lifecycle_and_transient {
            check(text, CODE_CONNECTION_LIFECYCLE, true);
        }
        // A bare EOF is a lifecycle failure but never transient.
        check("eof", CODE_CONNECTION_LIFECYCLE, false);
    }

    /// A typed body failure whose text main didn't match starts no round and
    /// takes no transient code, but is recorded so the credential isn't
    /// cooled. Without the marker the same text cools it, as on main.
    #[test]
    fn response_body_without_matched_text_does_not_retry_but_skips_cooldown() {
        let unmatched = [
            "end of file before message length reached",
            "error decoding response body",
            "peer closed connection without sending tls close_notify",
            "an existing connection was forcibly closed by the remote host. (os error 10054)",
            "an established connection was aborted by the software in your host. (os error 10053)",
            "x",
        ];
        for text in unmatched {
            let plain = decisions(&upstream(0, text));
            assert!(!plain.retry_round, "{text}");
            assert!(!plain.skips_cooldown, "{text}");
            assert!(plain.code.is_empty(), "{text}");

            let marked = decisions(&body_failed(0, text));
            assert!(!marked.transient, "{text}");
            assert!(!marked.lifecycle, "{text}");
            assert!(!marked.retry_round, "{text}");
            assert!(!marked.result_transient, "{text}");
            assert!(marked.result_lifecycle, "{text}");
            assert!(marked.skips_cooldown, "{text}");
            assert_eq!(marked.code, CODE_CONNECTION_LIFECYCLE, "{text}");
        }
    }

    /// The marker changes nothing once the failure carries a status, and
    /// nothing for a canceled call.
    #[test]
    fn response_body_with_a_status_is_decided_as_before() {
        let cases = [
            (200, "end of file before message length reached"),
            (502, "end of file before message length reached"),
            (502, "connection reset"),
            (429, "unexpected EOF during chunk size line"),
        ];
        for (status, text) in cases {
            assert_eq!(
                decisions(&body_failed(status, text)),
                decisions(&upstream(status, text)),
                "{status} {text}"
            );
        }
        let marked = ExecError::canceled().with_transport(TransportFault::ResponseBody);
        assert_eq!(decisions(&marked), decisions(&ExecError::canceled()));
    }

    /// A fault before any answer still counts by its type, as before.
    #[test]
    fn faults_before_a_response_keep_deciding_by_type() {
        let transient = decisions(&upstream(0, "x").with_transport(TransportFault::Transient));
        assert!(transient.retry_round);
        assert_eq!(transient.code, CODE_TRANSIENT_TRANSPORT);
        let lifecycle = decisions(&upstream(0, "x").with_transport(TransportFault::Lifecycle));
        assert!(lifecycle.retry_round);
        assert!(lifecycle.lifecycle);
        assert_eq!(lifecycle.code, CODE_CONNECTION_LIFECYCLE);
    }

    #[test]
    fn compact_requests() {
        let mut opts = Options::new(crate::exec::Format::from("openai-response"));
        opts.alt = "responses/compact".into();
        let err = upstream(405, "nope");
        assert!(is_responses_compact_request_fault_error(
            &opts,
            ErrView::Exec(&err)
        ));
        assert!(is_responses_compact_availability_neutral_error(
            &opts,
            ErrView::Exec(&err),
            None
        ));
        let limited = upstream(429, "nope");
        assert!(!is_responses_compact_availability_neutral_error(
            &opts,
            ErrView::Exec(&limited),
            None
        ));
    }
}
