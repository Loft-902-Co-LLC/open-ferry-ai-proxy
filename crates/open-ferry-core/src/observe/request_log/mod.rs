// Ported from CLIProxyAPI internal/logging/request_logger.go
// (FileRequestLogger, NewFileRequestLogger, IsEnabled, SetEnabled,
// SetErrorLogsMaxFiles), internal/api/middleware/response_writer.go
// (Finalize, shouldBufferResponseBody, hasActionableError,
// hasActionableAPIResponseErrors, isClientCancellationErrorMessage),
// internal/clienterror/client_error.go (IsClientCancellation) and
// internal/logging/cpa_trace.go (FormatCPATraceID) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The request log: a file per request, with the client's request, each
//! upstream attempt and the answer, or, with `request-log` off, a file per
//! request that failed (upstream's internal/logging/request_logger*.go).
//!
//! The binary makes a [`RequestLogger`] at start and [`reconfigure`]s it on
//! every config load. The server's capture layer [`starts`] each request it
//! logs, which fixes its [`Mode`]: [`Mode::Full`] with `request-log` on,
//! else [`Mode::ErrorsOnly`]. Each call the request makes asks the logger
//! for a [`Tap`], which records the upstream attempts in the request's
//! [`RequestState`] (see the `attempts` module). Once the answer is sent,
//! the layer [`finish`]es the request with what it kept of the client's
//! request ([`Downstream`]) and of the answer ([`Answer`]); a WebSocket
//! session's request is finished when the session ends ([`finish_later`]).
//! In [`Mode::ErrorsOnly`], only a request with an actionable error
//! ([`has_actionable_error`]) is written, as `error-*.log`, and the oldest
//! of those are deleted beyond `error-logs-max-files`.
//!
//! Nothing is written on the request's task: a finished request is handed
//! to one writer thread through a bounded queue, which formats, scrubs and
//! writes it. A request that finds the queue full is not logged; the drops
//! are counted and warned about at most once a minute.
//!
//! [`starts`]: RequestLogger::start
//!
//! Deviations from upstream:
//! - On disk, every credential header is masked (`Authorization`,
//!   `X-Api-Key`, `X-Goog-Api-Key`, `Cookie`, `Set-Cookie`,
//!   `X-Management-Key`, `Proxy-Authorization` and any other name
//!   [`mask::is_credential_header`] knows), the answer's included; an
//!   upstream URL's user info and key-like query parameters are masked,
//!   and a credential of one or two bytes is hidden whole; and every copy
//!   of every secret known, however short, is scrubbed from the whole file
//!   and from the path its name is made from (see [`redact`]): those the
//!   attempts sent (their credential headers, cookies, URLs, proxies and
//!   credentials), those of the client's credential headers, cookies and
//!   URL and of the answers' headers, and the client's key. Upstream writes
//!   bodies, upstream URLs and names as they are.
//! - The mode is fixed when the request arrives, where upstream reads
//!   `request-log` again when it finishes; commercial mode is read live,
//!   where upstream reads it only at start.
//! - Bodies are kept in memory up to [`CAPTURE_LIMIT`] bytes each instead
//!   of spilled to temporary files, so there are no spill files to purge at
//!   start; what is left out is counted in one line at the end of the log.
//! - With `request-log` off, the client's body is kept as the handler reads
//!   it, whatever its size, where upstream reads a body of up to 1 MiB
//!   ahead of the handler.
//! - The downstream WebSocket timeline isn't kept: a Responses WebSocket
//!   session's log has its upgrade request, the upstream attempts of all
//!   its turns, and a `101` answer.
//! - Files are written by a writer thread after the answer is sent; one
//!   that finds the queue full is dropped.
//!
//! [`mask::is_credential_header`]: crate::observe::mask::is_credential_header
//! [`redact`]: crate::observe::redact

mod attempts;
mod body_source;
mod format;
mod names;
mod writer;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use chrono::{DateTime, Local, TimeZone};
use http::HeaderMap;

pub use body_source::{CAPTURE_LIMIT, DeferredCapture, ResponseCapture};
pub use names::{
    LogFileMeta, MAX_SANITIZED, is_error_log_name, log_file_is_newer, parse_log_metadata,
    sanitize_for_filename,
};

use super::redact::Secrets;
use super::{RequestContext, Tap, mask};
use crate::config::Config;
use attempts::{Attempts, RequestLogTap};

#[cfg(test)]
mod tests;

/// The response header that tells the client which credential served it
/// (upstream's `CPATraceIDHeader`).
pub const CPA_TRACE_ID_HEADER: &str = "X-CPA-TRACE-ID";

/// The status a request the client gave up on is logged with (upstream's
/// `StatusClientClosedRequest`).
pub const STATUS_CLIENT_CLOSED_REQUEST: u16 = 499;

/// The trace ID of a request answered with the credential of index
/// `auth_index`, picked at `selected_at` (upstream's `FormatCPATraceID`):
/// `<yyyymmddHHMMSS>-<auth index>-<request ID>`, or empty when a part is
/// missing.
pub fn format_cpa_trace_id<Tz: TimeZone>(
    selected_at: Option<&DateTime<Tz>>,
    auth_index: &str,
    request_id: &str,
) -> String
where
    Tz::Offset: fmt::Display,
{
    let auth_index = auth_index.trim();
    let request_id = request_id.trim();
    match selected_at {
        Some(at) if !auth_index.is_empty() && !request_id.is_empty() => {
            format!("{}-{auth_index}-{request_id}", at.format("%Y%m%d%H%M%S"))
        }
        _ => String::new(),
    }
}

/// The trace ID of the request of `context`, from the credential its
/// latest call was given (see [`format_cpa_trace_id`]), or `None` when it
/// was given none, or one without an index.
pub fn trace_id(context: &RequestContext) -> Option<String> {
    let selected = context.selected()?;
    let selected_at = selected.selected_at.with_timezone(&Local);
    let trace = format_cpa_trace_id(Some(&selected_at), selected.index(), context.id.as_str());
    (!trace.is_empty()).then_some(trace)
}

/// How much of a request is logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `request-log` is on: every request is written whole.
    Full,
    /// `request-log` is off: a request is written only when it failed.
    ErrorsOnly,
}

impl Mode {
    /// Whether an answer with `status` is kept for the log (upstream's
    /// `shouldBufferResponseBody`): always in [`Mode::Full`], else when it
    /// is an error the client didn't cause by leaving.
    pub fn keeps_answer(self, status: u16) -> bool {
        match self {
            Self::Full => true,
            Self::ErrorsOnly => status >= 400 && status != STATUS_CLIENT_CLOSED_REQUEST,
        }
    }
}

/// An error a handler gave the client, as the log's `=== API ERROR
/// RESPONSE ===` section shows it (upstream's `ErrorMessage`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    /// The status given to the client.
    pub status: u16,
    /// The error's text.
    pub message: String,
    /// Whether it came from the client's request being canceled.
    pub canceled: bool,
}

impl ApiError {
    /// Whether it came from the client leaving (upstream's
    /// `isClientCancellationErrorMessage`).
    pub fn is_client_cancellation(&self) -> bool {
        is_client_cancellation(self.status, &self.message, self.canceled)
    }
}

/// Whether an error with `status` and `message` came from the client
/// leaving (upstream's `IsClientCancellation`): status 499, a canceled
/// request, or a message that says so.
pub fn is_client_cancellation(status: u16, message: &str, canceled: bool) -> bool {
    if status == STATUS_CLIENT_CLOSED_REQUEST || canceled {
        return true;
    }
    let lower = message.to_lowercase();
    lower.contains("context canceled") || lower.contains("client closed request")
}

/// Whether a request answered with `status`, `canceled` when the client
/// left, with the handlers' `api_errors`, failed in a way worth an error
/// log (upstream's `hasActionableError`): any error not caused by the
/// client leaving, else an error status other than 499, unless the client
/// left before an error.
pub fn has_actionable_error(status: u16, canceled: bool, api_errors: &[ApiError]) -> bool {
    if api_errors
        .iter()
        .any(|error| !error.is_client_cancellation())
    {
        return true;
    }
    if status == STATUS_CLIENT_CLOSED_REQUEST {
        return false;
    }
    if canceled && status < 400 {
        return false;
    }
    status >= 400
}

/// The logger itself, shared by its handles.
struct Logger {
    dir: PathBuf,
    enabled: AtomicBool,
    commercial: AtomicBool,
    max_error_files: AtomicI64,
    writer: writer::Writer,
}

/// The request logger (upstream's `FileRequestLogger`). Cloning gives
/// another handle to the same logger; the default one logs nothing.
#[derive(Clone, Default)]
pub struct RequestLogger {
    inner: Option<Arc<Logger>>,
}

impl RequestLogger {
    /// A logger for `config`, writing to `log_dir`, which is taken from the
    /// directory of `config_path` when relative (upstream's
    /// `NewFileRequestLogger`, as `defaultRequestLoggerFactory` calls it).
    pub fn new(config: &Config, log_dir: &Path, config_path: &Path) -> Self {
        let dir = match config_path.parent() {
            Some(parent) if log_dir.is_relative() && !parent.as_os_str().is_empty() => {
                parent.join(log_dir)
            }
            _ => log_dir.to_path_buf(),
        };
        Self {
            inner: Some(Arc::new(Logger {
                dir,
                enabled: AtomicBool::new(config.request_log),
                commercial: AtomicBool::new(config.commercial_mode),
                max_error_files: AtomicI64::new(config.error_logs_max_files),
                writer: writer::Writer::default(),
            })),
        }
    }

    /// Whether every request is logged (upstream's `IsEnabled`).
    pub fn is_enabled(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|logger| logger.enabled.load(Ordering::Relaxed))
    }

    /// The directory the logs go to, unless this logger logs nothing.
    pub fn dir(&self) -> Option<&Path> {
        self.inner.as_ref().map(|logger| logger.dir.as_path())
    }

    /// Starts logging the request of `context`, and gives the mode it is
    /// logged in, or `None` when it isn't (a logger that logs nothing, or
    /// commercial mode). A request already started keeps its mode.
    pub fn start(&self, context: &RequestContext) -> Option<Mode> {
        let logger = self.inner.as_ref()?;
        if logger.commercial.load(Ordering::Relaxed) {
            return None;
        }
        let mode = if logger.enabled.load(Ordering::Relaxed) {
            Mode::Full
        } else {
            Mode::ErrorsOnly
        };
        let mut capture = context.request_log().lock();
        Some(
            capture
                .get_or_insert_with(|| Capture {
                    logger: Arc::clone(logger),
                    mode,
                    request_id: context.id.as_str().to_owned(),
                    arrived_at: context.started_at.with_timezone(&Local),
                    attempts: Attempts::default(),
                    api_errors: Vec::new(),
                    secrets: Secrets::new(),
                    pending: None,
                })
                .mode,
        )
    }

    /// The tap that records the upstream attempts of a call made for the
    /// request of `context`, or `None` when they aren't recorded: the
    /// request isn't logged, or commercial mode is on. Every call a request
    /// makes asks for one, the Alpha Search pass-through included.
    pub fn tap(&self, context: &Arc<RequestContext>) -> Option<Arc<dyn Tap>> {
        let logger = self.inner.as_ref()?;
        if logger.commercial.load(Ordering::Relaxed) {
            return None;
        }
        context.request_log().mode()?;
        Some(Arc::new(RequestLogTap::new(Arc::clone(context))))
    }

    /// Waits until the writer has written every log handed to it so far.
    /// For tests and shutdown; never call it on a request's task.
    pub fn flush(&self) {
        if let Some(logger) = &self.inner {
            logger.writer.flush();
        }
    }
}

impl fmt::Debug for RequestLogger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            Some(logger) => f
                .debug_struct("RequestLogger")
                .field("dir", &logger.dir)
                .field("enabled", &logger.enabled.load(Ordering::Relaxed))
                .field("commercial", &logger.commercial.load(Ordering::Relaxed))
                .field(
                    "max_error_files",
                    &logger.max_error_files.load(Ordering::Relaxed),
                )
                .finish(),
            None => f.write_str("RequestLogger(off)"),
        }
    }
}

/// Applies `config` to `logger` (upstream's `SetEnabled` and
/// `SetErrorLogsMaxFiles` on reload, and the commercial mode, which
/// upstream reads only at start). `previous` is the config before, `None`
/// at start.
pub fn reconfigure(logger: &RequestLogger, previous: Option<&Config>, config: &Config) {
    let _ = previous;
    let Some(logger) = &logger.inner else {
        return;
    };
    logger.enabled.store(config.request_log, Ordering::Relaxed);
    logger
        .commercial
        .store(config.commercial_mode, Ordering::Relaxed);
    logger
        .max_error_files
        .store(config.error_logs_max_files, Ordering::Relaxed);
}

/// What was kept of the client's request: the URL with its query masked,
/// the method, the headers and the body.
pub struct Downstream {
    /// The path, and the query masked (see [`Downstream::url`]).
    pub url: String,
    /// The secrets of its URL (see [`Downstream::url_secrets`]), to scrub
    /// from the log; those of its headers are gathered when it is written.
    pub secrets: Secrets,
    /// The method.
    pub method: String,
    /// The headers, as they came; they are masked when written.
    pub headers: HeaderMap,
    /// The body.
    pub body: RequestBody,
}

impl Downstream {
    /// The URL a log shows for `path` and `query` (upstream's
    /// `captureRequestInfo`): the query's key-like values masked.
    pub fn url(path: &str, query: Option<&str>) -> String {
        let masked = mask::mask_sensitive_query(query.unwrap_or_default());
        if masked.is_empty() {
            path.to_owned()
        } else {
            format!("{path}?{masked}")
        }
    }

    /// The secrets of the URL of `path` and `query`: the values of its
    /// key-like query parameters (see [`Secrets::add_url`]).
    pub fn url_secrets(path: &str, query: Option<&str>) -> Secrets {
        let mut secrets = Secrets::new();
        if let Some(query) = query {
            secrets.add_url(&format!("{path}?{query}"));
        }
        secrets
    }
}

impl fmt::Debug for Downstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Downstream")
            .field("url", &self.url)
            .field("method", &self.method)
            .field("body", &self.body)
            .finish_non_exhaustive()
    }
}

/// What was kept of the client's body.
#[derive(Debug)]
pub enum RequestBody {
    /// Nothing.
    None,
    /// The body, read ahead of the handler; `truncated` when it had more.
    Captured {
        /// What was read, as it came.
        raw: Bytes,
        /// Whether the body went on past it.
        truncated: bool,
    },
    /// What the handler read of it.
    Deferred(DeferredCapture),
}

/// What was kept of the answer.
#[derive(Debug)]
pub struct Answer {
    /// The status.
    pub status: u16,
    /// The headers, as they were sent; they are masked when written.
    pub headers: HeaderMap,
    /// The body, as it was sent.
    pub body: ResponseCapture,
    /// When the head was sent.
    pub head_at: DateTime<Local>,
    /// Whether the client left before the answer ended.
    pub canceled: bool,
}

impl Answer {
    /// An answer with `status` and `headers` whose head is sent now,
    /// nothing of its body kept yet.
    pub fn new(status: u16, headers: HeaderMap) -> Self {
        Self {
            status,
            headers,
            body: ResponseCapture::default(),
            head_at: Local::now(),
            canceled: false,
        }
    }
}

/// Finishes the log of the request of `context` with what was kept of its
/// request and answer, and hands it to the writer, or drops it when it isn't
/// to be written (upstream's `Finalize`). A request finished already, or
/// never started, is left alone.
pub fn finish(context: &RequestContext, downstream: Downstream, answer: Answer) {
    let capture = context.request_log().lock().take();
    if let Some(mut capture) = capture {
        capture.add_secret(context.client_key());
        capture.submit(downstream, answer);
    }
}

/// Finishes the log of the request of `context` once its context is
/// dropped, as a WebSocket session's is when the session ends: the
/// attempts of all its turns are written with it.
pub fn finish_later(context: &RequestContext, downstream: Downstream, answer: Answer) {
    let mut capture = context.request_log().lock();
    if let Some(capture) = capture.as_mut() {
        capture.add_secret(context.client_key());
        capture.pending = Some((downstream, answer));
    }
}

/// What the request log keeps of one request while it runs, in its
/// [`RequestContext`]: the server's capture layer and the request's taps
/// share it.
#[derive(Default)]
pub struct RequestState {
    capture: Mutex<Option<Capture>>,
}

impl RequestState {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Capture>> {
        self.capture.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The mode the request is logged in, or `None` when it isn't logged
    /// or is finished.
    pub fn mode(&self) -> Option<Mode> {
        self.lock().as_ref().map(|capture| capture.mode)
    }

    /// Whether the request is logged whole.
    pub(crate) fn is_full(&self) -> bool {
        self.mode() == Some(Mode::Full)
    }

    /// Runs `f` on the capture while the request is logged.
    pub(crate) fn with(&self, f: impl FnOnce(&mut Capture)) {
        if let Some(capture) = self.lock().as_mut() {
            f(capture);
        }
    }

    /// Records an error a handler gave the client, for the log's `=== API
    /// ERROR RESPONSE ===` sections (upstream's `API_RESPONSE_ERROR`, which
    /// handlers record only with `request-log` on). `canceled` says the
    /// client's request was canceled.
    pub fn record_api_error(&self, status: u16, message: &str, canceled: bool) {
        self.with(|capture| {
            if capture.mode == Mode::Full {
                capture.api_errors.push(ApiError {
                    status,
                    message: message.to_owned(),
                    canceled,
                });
            }
        });
    }

    /// The errors recorded so far with [`Self::record_api_error`] (what
    /// upstream keeps as `API_RESPONSE_ERROR`).
    pub fn api_errors(&self) -> Vec<ApiError> {
        self.lock()
            .as_ref()
            .map(|capture| capture.api_errors.clone())
            .unwrap_or_default()
    }
}

impl fmt::Debug for RequestState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestState")
            .field("mode", &self.mode())
            .finish_non_exhaustive()
    }
}

impl Drop for RequestState {
    fn drop(&mut self) {
        let capture = self
            .capture
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(mut capture) = capture
            && let Some((downstream, answer)) = capture.pending.take()
        {
            capture.submit(downstream, answer);
        }
    }
}

/// What is kept of a request being logged.
pub(crate) struct Capture {
    logger: Arc<Logger>,
    pub(crate) mode: Mode,
    request_id: String,
    arrived_at: DateTime<Local>,
    pub(crate) attempts: Attempts,
    api_errors: Vec<ApiError>,
    secrets: Secrets,
    pending: Option<(Downstream, Answer)>,
}

impl Capture {
    /// Keeps `secrets` to scrub from the log.
    pub(crate) fn add_secrets(&mut self, secrets: &Secrets) {
        self.secrets.extend(secrets);
    }

    /// Keeps the secrets of `headers` to scrub from the log.
    pub(crate) fn add_header_secrets(&mut self, headers: &HeaderMap) {
        self.secrets.add_headers(headers);
    }

    fn add_secret(&mut self, secret: Option<&str>) {
        if let Some(secret) = secret {
            self.secrets.add(secret);
        }
    }

    /// Hands the log to the writer, if it is to be written.
    fn submit(self, downstream: Downstream, answer: Answer) {
        let logger = self.logger;
        if logger.commercial.load(Ordering::Relaxed) {
            return;
        }
        let forced = match self.mode {
            Mode::Full => false,
            Mode::ErrorsOnly => {
                if !has_actionable_error(answer.status, answer.canceled, &self.api_errors) {
                    return;
                }
                true
            }
        };
        logger.writer.submit(writer::Entry {
            dir: logger.dir.clone(),
            forced,
            max_error_files: logger.max_error_files.load(Ordering::Relaxed),
            full: self.mode == Mode::Full,
            request_id: self.request_id,
            arrived_at: self.arrived_at,
            downstream,
            answer,
            attempts: self.attempts,
            api_errors: self.api_errors,
            secrets: self.secrets,
        });
    }
}
