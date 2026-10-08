// Ported from CLIProxyAPI sdk/cliproxy/auth/errors.go, the modelCooldownError
// in sdk/cliproxy/auth/selector.go, SafeResponseHeaders in
// sdk/cliproxy/auth/home_concurrency.go, HTTPStatusFromError in
// internal/clienterror/client_error.go and
// sdk/cliproxy/executor/websocket.go (UpstreamWebsocketReplayRequiredError,
// NewUpstreamWebsocketReplayRequiredError), with the UnsupportedPartError
// conversion and an error that keeps the answer's usage (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use std::fmt;
use std::time::Duration;

use http::{HeaderMap, HeaderValue, header};
use open_ferry_translate::registry::UnsupportedPartError;
use serde_json::{Map, Value, json};

/// What failed, as far as the HTTP layer needs to know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// An executor's error, usually the provider's status and body.
    Upstream,
    /// No credential serves the model (`auth_not_found`).
    AuthNotFound,
    /// Every credential that serves the model is unavailable for now
    /// (`auth_unavailable`).
    AuthUnavailable,
    /// The call named no provider (`provider_not_found`).
    ProviderNotFound,
    /// A stream ended before its first chunk (`empty_stream`).
    EmptyStream,
    /// Every credential for the model is cooling down.
    ModelCooldown,
    /// The client went away (upstream's `context.Canceled`).
    Canceled,
    /// A deadline passed (upstream's `context.DeadlineExceeded`).
    DeadlineExceeded,
    /// No executor is registered for the credential's provider
    /// (`executor_not_found`).
    ExecutorNotFound,
}

impl ErrorKind {
    /// The code upstream's auth manager gives the error, if it has one.
    pub fn code(self) -> Option<&'static str> {
        Some(match self {
            Self::AuthNotFound => "auth_not_found",
            Self::AuthUnavailable => "auth_unavailable",
            Self::ProviderNotFound => "provider_not_found",
            Self::EmptyStream => "empty_stream",
            Self::ExecutorNotFound => "executor_not_found",
            _ => return None,
        })
    }

    /// Whether the error says no credential could take the call
    /// (upstream's `isAuthSelectionUnavailable`).
    pub fn is_auth_selection(self) -> bool {
        matches!(self, Self::AuthNotFound | Self::AuthUnavailable)
    }
}

/// The body of [`ExecError::replay_required`]
/// (`UpstreamWebsocketReplayRequiredError`).
const REPLAY_REQUIRED_BODY: &str = r#"{"error":{"message":"upstream transport requires full HTTP replay","type":"server_error","code":"upstream_http_replay_required","status":426}}"#;

/// How the Responses WebSocket closes after an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WsClose {
    /// The client must resend the conversation (close code 1012).
    ReplayRequired,
    /// A message was too big (close code 1009), with the reason.
    MessageTooBig(String),
}

/// A connection failure the provider never answered, for executors that can
/// tell; upstream recognizes these by their error text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportFault {
    /// A network error that may clear on its own, such as a refused or reset
    /// connection or a TLS handshake timeout.
    Transient,
    /// The connection or call ended early, such as an unexpected EOF or a
    /// WebSocket closing.
    Lifecycle,
}

/// A failed call.
#[derive(Clone, Debug)]
pub struct ExecError {
    /// What failed.
    pub kind: ErrorKind,
    /// The HTTP status, or 0 for none.
    pub status: u16,
    /// What went wrong. For an [`ErrorKind::Upstream`] error this is usually
    /// the provider's body; for the auth manager's own errors, the message
    /// after the code.
    pub message: String,
    /// A summary of the last provider error behind an auth manager error.
    pub cause: Option<String>,
    /// Response headers that came with the error (upstream's `Headers()`).
    pub headers: HeaderMap,
    /// When a credential frees up, for [`ErrorKind::AuthUnavailable`] and
    /// [`ErrorKind::ModelCooldown`].
    pub retry_after: Option<Duration>,
    /// Whether, and how, the connection to the provider failed.
    pub transport: Option<TransportFault>,
    /// Whether the provider rejected the credential for good, so the client
    /// must sign in again.
    pub terminal_auth: bool,
    /// How the Responses WebSocket closes after this error, when it doesn't
    /// close the usual way.
    pub ws_close: Option<WsClose>,
    /// Whether the failure is the credential's as a whole, such as a usage
    /// limit across all its models (upstream's `IsCredentialScoped`).
    pub credential_scoped: bool,
    /// Whether the failure is this request's only, so the credential stays
    /// usable (upstream's `IsRequestScoped`).
    pub request_scoped: bool,
    /// Whether the provider answered and the answer's usage counts though
    /// the call failed, as when its `apply_patch` call couldn't be carried
    /// over: the failure's usage record keeps the counts the answer gave
    /// (upstream's `StreamUsageBuffer.PublishFailure` with what it observed).
    pub keeps_usage: bool,
}

impl ExecError {
    /// An error of `kind` with `message` and no status.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            status: 0,
            message: message.into(),
            cause: None,
            headers: HeaderMap::new(),
            retry_after: None,
            transport: None,
            terminal_auth: false,
            ws_close: None,
            credential_scoped: false,
            request_scoped: false,
            keeps_usage: false,
        }
    }

    /// A provider's error: its status and body.
    pub fn upstream(status: u16, body: impl Into<String>) -> Self {
        Self::new(ErrorKind::Upstream, body).with_status(status)
    }

    /// No credential serves the model.
    pub fn auth_not_found() -> Self {
        Self::new(ErrorKind::AuthNotFound, "no auth available")
    }

    /// Every credential that serves the model is unavailable for
    /// `retry_after`, so the client gets 503 and a `Retry-After`.
    pub fn auth_unavailable(retry_after: Duration) -> Self {
        let mut error = Self::new(ErrorKind::AuthUnavailable, "no auth available").with_status(503);
        error.retry_after = Some(retry_after);
        error
    }

    /// A stream ended before its first chunk.
    pub fn empty_stream() -> Self {
        Self::new(
            ErrorKind::EmptyStream,
            "upstream stream closed before first payload",
        )
    }

    /// The client went away.
    pub fn canceled() -> Self {
        Self::new(ErrorKind::Canceled, "context canceled")
    }

    /// The request can't go on the upstream WebSocket it was sent for, so
    /// the client must replay the turn over a new socket: a 426 that closes
    /// the Responses WebSocket with 1012, and leaves the credential usable
    /// (`NewUpstreamWebsocketReplayRequiredError`).
    pub fn replay_required() -> Self {
        let mut error = Self::upstream(426, REPLAY_REQUIRED_BODY).with_request_scoped();
        error.ws_close = Some(WsClose::ReplayRequired);
        error
    }

    /// Every credential for `model` is cooling down for `reset_in`. The
    /// message is upstream's JSON body, and the status 429.
    pub fn model_cooldown(
        model: &str,
        provider: Option<&str>,
        reset_in: Duration,
        cause: Option<&str>,
    ) -> Self {
        let model_name = if model.is_empty() {
            "requested model"
        } else {
            model
        };
        let mut message = format!("All credentials for model {model_name} are cooling down");
        if let Some(provider) = provider.filter(|p| !p.is_empty()) {
            message.push_str(&format!(" via provider {provider}"));
        }
        let reset_seconds = ceil_seconds(reset_in);
        let display = if reset_in > Duration::ZERO && reset_in < Duration::from_secs(1) {
            Duration::from_secs(1)
        } else {
            round_to_second(reset_in)
        };
        let cause = cause.filter(|c| !c.is_empty());
        if let Some(cause) = cause {
            message.push_str(&format!(" (last error: {cause})"));
        }
        // Keys in the order Go's map marshalling sorts them.
        let mut body = Map::new();
        body.insert("code".into(), "model_cooldown".into());
        if let Some(cause) = cause {
            body.insert("last_upstream_error".into(), cause.into());
        }
        body.insert("message".into(), message.into());
        body.insert("model".into(), model.into());
        if let Some(provider) = provider.filter(|p| !p.is_empty()) {
            body.insert("provider".into(), provider.into());
        }
        body.insert("reset_seconds".into(), reset_seconds.into());
        body.insert("reset_time".into(), go_duration(display).into());
        let text = json!({ "error": Value::Object(body) }).to_string();

        let mut error = Self::new(ErrorKind::ModelCooldown, text).with_status(429);
        error.headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        error
            .headers
            .insert(header::RETRY_AFTER, HeaderValue::from(reset_seconds));
        error.retry_after = Some(reset_in);
        error
    }

    /// Sets the HTTP status.
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    /// Sets the summary of the provider error behind this one.
    pub fn with_cause(mut self, cause: impl Into<String>) -> Self {
        self.cause = Some(cause.into());
        self
    }

    /// Marks the error as a failed connection to the provider.
    pub fn with_transport(mut self, fault: TransportFault) -> Self {
        self.transport = Some(fault);
        self
    }

    /// Marks the error as the provider rejecting the credential for good.
    pub fn with_terminal_auth(mut self) -> Self {
        self.terminal_auth = true;
        self
    }

    /// Marks the failure as the credential's as a whole.
    pub fn with_credential_scoped(mut self) -> Self {
        self.credential_scoped = true;
        self
    }

    /// Marks the failure as this request's only.
    pub fn with_request_scoped(mut self) -> Self {
        self.request_scoped = true;
        self
    }

    /// Marks the failure as one whose answer's usage counts.
    pub fn with_usage_kept(mut self) -> Self {
        self.keeps_usage = true;
        self
    }

    /// The HTTP status to answer with, or 0 when the error has none
    /// (upstream's `HTTPStatusFromError`).
    pub fn http_status(&self) -> u16 {
        match self.kind {
            _ if self.status > 0 => self.status,
            ErrorKind::Canceled => 499,
            ErrorKind::DeadlineExceeded => 504,
            _ => 0,
        }
    }

    /// The `Retry-After` value to send the client, for the errors upstream
    /// trusts to set one (`SafeResponseHeaders`): a model cooldown, or every
    /// credential being unavailable for a while.
    pub fn retry_after_header(&self) -> Option<HeaderValue> {
        match self.kind {
            ErrorKind::ModelCooldown => self.headers.get(header::RETRY_AFTER).cloned(),
            ErrorKind::AuthUnavailable => {
                let wait = self.retry_after.filter(|wait| !wait.is_zero())?;
                Some(HeaderValue::from(ceil_seconds(wait).max(1)))
            }
            _ => None,
        }
    }
}

impl fmt::Display for ExecError {
    /// The error's text, as upstream's `Error()` gives it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(code) = self.kind.code() else {
            return f.write_str(&self.message);
        };
        let base = format!("{code}: {}", self.message);
        match self.cause.as_deref() {
            Some(cause) if !cause.is_empty() && !base.contains(cause) => {
                write!(f, "{base} (last upstream error: {cause})")
            }
            _ => f.write_str(&base),
        }
    }
}

impl std::error::Error for ExecError {}

impl From<UnsupportedPartError> for ExecError {
    /// A request a translator refused, for a content part the provider
    /// can't receive: a 400 naming the part, which leaves the credential
    /// usable (upstream's `UnsupportedPartError`, which executors return
    /// before calling the provider).
    fn from(error: UnsupportedPartError) -> Self {
        Self::upstream(error.status_code(), error.to_string()).with_request_scoped()
    }
}

/// Whole seconds, rounded up.
fn ceil_seconds(duration: Duration) -> u64 {
    duration.as_secs() + u64::from(duration.subsec_nanos() > 0)
}

/// Rounded to the nearest second, halves away from zero, as Go's
/// `Duration.Round(time.Second)`.
fn round_to_second(duration: Duration) -> Duration {
    let up = duration.subsec_nanos() >= 500_000_000;
    Duration::from_secs(duration.as_secs() + u64::from(up))
}

/// A whole number of seconds as Go's `Duration.String` writes it: `0s`,
/// `45s`, `1m0s`, `1h2m3s`.
fn go_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    match (hours, minutes) {
        (0, 0) => format!("{seconds}s"),
        (0, _) => format!("{minutes}m{seconds}s"),
        _ => format!("{hours}h{minutes}m{seconds}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_matches_upstream_error_strings() {
        assert_eq!(
            ExecError::auth_not_found().to_string(),
            "auth_not_found: no auth available"
        );
        let caused = ExecError::auth_not_found().with_cause("rate_limit_exceeded: slow down");
        assert_eq!(
            caused.to_string(),
            "auth_not_found: no auth available (last upstream error: rate_limit_exceeded: slow down)"
        );
        // A cause the message already holds isn't repeated.
        let mut repeated = ExecError::auth_not_found().with_cause("no auth");
        assert_eq!(repeated.to_string(), "auth_not_found: no auth available");
        repeated.cause = Some(String::new());
        assert_eq!(repeated.to_string(), "auth_not_found: no auth available");
        assert_eq!(
            ExecError::upstream(400, "{\"a\":1}").to_string(),
            "{\"a\":1}"
        );
        assert_eq!(
            ExecError::empty_stream().to_string(),
            "empty_stream: upstream stream closed before first payload"
        );
    }

    /// `TestExecutionErrorMessage_UnsupportedPartKeepsNameAnd400`, at the
    /// executor's error: a refused part keeps its name and answers 400,
    /// and the credential stays usable.
    #[test]
    fn unsupported_part_keeps_its_name_and_400() {
        let error = ExecError::from(UnsupportedPartError::new("container_upload"));
        assert_eq!(error.http_status(), 400);
        assert_eq!(
            error.to_string(),
            "unsupported content part: container_upload"
        );
        assert!(error.request_scoped && !error.credential_scoped);
    }

    #[test]
    fn statuses_fall_back_for_cancellation_and_deadlines() {
        assert_eq!(ExecError::canceled().http_status(), 499);
        assert_eq!(
            ExecError::new(ErrorKind::DeadlineExceeded, "x").http_status(),
            504
        );
        assert_eq!(ExecError::canceled().with_status(502).http_status(), 502);
        assert_eq!(ExecError::auth_not_found().http_status(), 0);
    }

    #[test]
    fn model_cooldown_body_matches_upstream() {
        let error = ExecError::model_cooldown(
            "gpt-5",
            Some("codex"),
            Duration::from_millis(59_400),
            Some("rate_limit_exceeded: slow down"),
        );
        assert_eq!(error.http_status(), 429);
        assert_eq!(
            error.to_string(),
            r#"{"error":{"code":"model_cooldown","last_upstream_error":"rate_limit_exceeded: slow down","message":"All credentials for model gpt-5 are cooling down via provider codex (last error: rate_limit_exceeded: slow down)","model":"gpt-5","provider":"codex","reset_seconds":60,"reset_time":"59s"}}"#
        );
        assert_eq!(error.retry_after_header().unwrap(), "60");

        let bare = ExecError::model_cooldown("", None, Duration::from_millis(200), None);
        assert_eq!(
            bare.to_string(),
            r#"{"error":{"code":"model_cooldown","message":"All credentials for model requested model are cooling down","model":"","reset_seconds":1,"reset_time":"1s"}}"#
        );
    }

    #[test]
    fn go_durations_print_as_go_does() {
        assert_eq!(go_duration(Duration::ZERO), "0s");
        assert_eq!(go_duration(Duration::from_secs(45)), "45s");
        assert_eq!(go_duration(Duration::from_secs(60)), "1m0s");
        assert_eq!(go_duration(Duration::from_secs(3723)), "1h2m3s");
        assert_eq!(go_duration(Duration::from_secs(7200)), "2h0m0s");
        assert_eq!(round_to_second(Duration::from_millis(1500)).as_secs(), 2);
        assert_eq!(round_to_second(Duration::from_millis(1499)).as_secs(), 1);
    }

    #[test]
    fn retry_after_comes_only_from_trusted_errors() {
        let unavailable = ExecError::auth_unavailable(Duration::from_millis(1200));
        assert_eq!(unavailable.retry_after_header().unwrap(), "2");
        let short = ExecError::auth_unavailable(Duration::from_millis(10));
        assert_eq!(short.retry_after_header().unwrap(), "1");
        let mut upstream = ExecError::upstream(429, "slow");
        upstream.retry_after = Some(Duration::from_secs(5));
        upstream
            .headers
            .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
        assert!(upstream.retry_after_header().is_none());
    }
}
