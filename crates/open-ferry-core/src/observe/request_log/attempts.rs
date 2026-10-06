// Ported from CLIProxyAPI internal/runtime/executor/helps/logging_helpers.go
// (RecordAPIRequest, deferAPIRequest, newAPIRequestLogBuilder,
// RecordAPIResponseMetadata, RecordAPIResponseError, AppendAPIResponseChunk,
// RecordAPIWebsocketRequest, AppendAPIWebsocketResponse,
// AppendCodexAPIWebsocketResponse, RecordAPIWebsocketError, ensureAttempt,
// ensureResponseIntro, writeAttemptResponse, updateAggregatedRequest,
// updateAggregatedResponse, appendAPIWebsocketTimeline, formatAuthInfo)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The upstream attempts of a request, as its log shows them, and the
//! [`Tap`] that records them.
//!
//! With the request log on, each attempt gets an `=== API REQUEST n ===`
//! block, written when the log is, and an `=== API RESPONSE n ===` block,
//! built as its answer comes: the status and headers, then the body, a
//! line of a stream at a time, and the error when the attempt failed. A
//! message on an upstream WebSocket goes to the API WebSocket timeline
//! instead, with the call's error; but an error the executor tells as the
//! attempt's ([`Tap::attempt_error`]) goes to an API RESPONSE block, as
//! upstream's executors record an empty `response.incomplete`. With it off,
//! the tap keeps, for the error log of a request that fails, each upstream
//! request, each answer's status and headers, the body of an answer with an
//! error status, and each attempt's error; not the body of a successful
//! answer, nor anything of an upstream WebSocket.
//!
//! Deviations from upstream:
//! - The tap sees what the executors report (see [`Tap`]), where upstream's
//!   executors write into the request's context themselves. A stream's
//!   chunks are split into lines here, as upstream's executors scan them;
//!   any other answer is one chunk.
//! - The call's error is written for an attempt that got no answer, as
//!   upstream's executors record every failed send. A failure after the
//!   answer's head is written as the executor tells it
//!   ([`Tap::attempt_error`]), where upstream's executors record one; the
//!   call's error that follows isn't written again.
//! - The upstream URL's user info and secret query parameters are masked,
//!   the headers' values masked as [`mask::mask_header_value`] masks them,
//!   and the bodies scrubbed of the attempt's secrets and those of the
//!   answer's headers when written; upstream writes the URL, the bodies and
//!   fewer masked headers as they are.
//! - With the request log off, the answers' status and headers, the bodies
//!   of those with an error status and the attempts' errors are kept, so an
//!   error log has an `=== API RESPONSE n ===` block for each attempt;
//!   upstream's executors record no answer with it off, and its error log
//!   has only the error the handler gave the client, in an `=== API
//!   RESPONSE ===` section, which the log's `=== RESPONSE ===` has here.
//! - The bodies of the upstream requests are kept up to [`CAPTURE_LIMIT`]
//!   bytes in all, the request log on or off, each one cut with upstream's
//!   deferred-request marker; the answers and the WebSocket timeline up to
//!   [`CAPTURE_LIMIT`] bytes each, the rest counted for the log's note.
//! - A WebSocket request event has no handshake event after it, its error
//!   event no `Stage:` line, and quota headers in a Codex WebSocket message
//!   aren't merged into the answer's headers.

use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use chrono::Local;
use http::HeaderMap;
use open_ferry_translate::go::trim_space;

use super::body_source::CAPTURE_LIMIT;
use super::format::{rfc3339_nano, write_headers};
use super::{Capture, Mode};
use crate::exec::ExecError;
use crate::observe::{AttemptKind, AttemptRequest, Outcome, RequestContext, Tap, mask, redact};

/// The time now, as the attempts show it.
fn now() -> String {
    rfc3339_nano(&Local::now())
}

/// `url` as a log shows it: its user info hidden, and the values of its
/// query parameters that hold a secret masked (see
/// [`mask::mask_sensitive_query`]).
pub(crate) fn mask_url(url: &str) -> String {
    let (rest, fragment) = match url.split_once('#') {
        Some((rest, fragment)) => (rest, Some(fragment)),
        None => (url, None),
    };
    let (base, query) = match rest.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (rest, None),
    };
    let mut out = match base.split_once("://") {
        Some((scheme, after)) => {
            let end = after.find(['/', '\\']).unwrap_or(after.len());
            match after.split_at_checked(end) {
                Some((authority, path)) => match authority.rsplit_once('@') {
                    Some((_, host)) => format!("{scheme}://{}@{host}{path}", redact::REDACTED),
                    None => base.to_owned(),
                },
                None => base.to_owned(),
            }
        }
        None => base.to_owned(),
    };
    if let Some(query) = query {
        out.push('?');
        out.push_str(&mask::mask_sensitive_query(query));
    }
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

/// Who an attempt was made as (upstream's `formatAuthInfo`): the provider,
/// the credential's ID and label, and its type, with an API key masked.
fn auth_info(request: &AttemptRequest<'_>) -> String {
    let mut parts = Vec::new();
    for (name, value) in [
        ("provider", request.provider),
        ("auth_id", request.auth.id.as_str()),
        ("label", request.auth.label.as_str()),
    ] {
        let value = value.trim();
        if !value.is_empty() {
            parts.push(format!("{name}={value}"));
        }
    }
    match request.auth.account_info() {
        Some(("api_key", value)) if !value.trim().is_empty() => parts.push(format!(
            "type=api_key value={}",
            mask::hide_log_key(value.trim())
        )),
        Some(("api_key", _)) => parts.push("type=api_key".to_owned()),
        Some(("oauth", _)) => parts.push("type=oauth".to_owned()),
        _ => {}
    }
    parts.join(", ")
}

/// An upstream request as it was sent, kept to be written with the log.
pub(crate) struct RequestRecord {
    index: usize,
    at: String,
    url: String,
    method: String,
    headers: HeaderMap,
    auth: String,
    body: Bytes,
    body_len: usize,
}

impl RequestRecord {
    /// Writes the request's `=== API REQUEST n ===` block (upstream's
    /// `newAPIRequestLogBuilder` and the body after it).
    fn render(&self, out: &mut Vec<u8>) {
        let mut head = format!(
            "=== API REQUEST {} ===\nTimestamp: {}\n",
            self.index, self.at
        );
        if self.url.is_empty() {
            head.push_str("Upstream URL: <unknown>\n");
        } else {
            head.push_str(&format!("Upstream URL: {}\n", mask_url(&self.url)));
        }
        if !self.method.is_empty() {
            head.push_str(&format!("HTTP Method: {}\n", self.method));
        }
        if !self.auth.is_empty() {
            head.push_str(&format!("Auth: {}\n", self.auth));
        }
        head.push_str("\nHeaders:\n");
        write_headers(&mut head, &self.headers);
        head.push_str("\nBody:\n");
        out.extend_from_slice(head.as_bytes());
        if self.body_len == 0 {
            out.extend_from_slice(b"<empty>");
        } else {
            out.extend_from_slice(&self.body);
            if self.body.len() < self.body_len {
                out.extend_from_slice(
                    format!(
                        "\n[API REQUEST BODY TRUNCATED: captured first {} bytes]",
                        self.body.len()
                    )
                    .as_bytes(),
                );
            }
        }
        out.extend_from_slice(b"\n\n");
    }
}

/// What may still be kept of some part of a log, and how much was not.
#[derive(Default)]
struct Budget {
    used: usize,
    dropped: usize,
}

impl Budget {
    /// What of `payload` fits.
    fn take<'a>(&mut self, payload: &'a [u8]) -> &'a [u8] {
        let kept = payload.len().min(CAPTURE_LIMIT.saturating_sub(self.used));
        self.used += kept;
        self.dropped = self.dropped.saturating_add(payload.len() - kept);
        payload.get(..kept).unwrap_or_default()
    }
}

/// One upstream attempt (upstream's `upstreamAttempt`).
struct Attempt {
    index: usize,
    /// The request; `None` for an answer that came without one.
    request: Option<RequestRecord>,
    response: Vec<u8>,
    intro_written: bool,
    status_written: bool,
    headers_written: bool,
    body_started: bool,
    body_has_content: bool,
    prev_was_sse_event: bool,
    error_written: bool,
    trailing_newlines: usize,
}

impl Attempt {
    fn new(index: usize, request: Option<RequestRecord>) -> Self {
        Self {
            index,
            request,
            response: Vec::new(),
            intro_written: false,
            status_written: false,
            headers_written: false,
            body_started: false,
            body_has_content: false,
            prev_was_sse_event: false,
            error_written: false,
            trailing_newlines: 0,
        }
    }

    /// Adds `payload` to the answer, as far as `budget` allows (upstream's
    /// `writeAttemptResponse`).
    fn write(&mut self, budget: &mut Budget, payload: &[u8]) {
        let payload = budget.take(payload);
        if payload.is_empty() {
            return;
        }
        let trailing = super::format::count_trailing_newlines(payload);
        self.trailing_newlines = if trailing == payload.len() {
            trailing + self.trailing_newlines
        } else {
            trailing
        };
        self.response.extend_from_slice(payload);
    }
}

/// The upstream attempts of one request.
#[derive(Default)]
pub(crate) struct Attempts {
    list: Vec<Attempt>,
    request_bytes: usize,
    responses: Budget,
    timeline: Vec<u8>,
    timeline_budget: Budget,
}

impl Attempts {
    /// `request` as it is kept, numbered `index`, its body cut to what is
    /// left of [`CAPTURE_LIMIT`].
    fn record(&mut self, index: usize, request: &AttemptRequest<'_>) -> RequestRecord {
        let room = CAPTURE_LIMIT.saturating_sub(self.request_bytes);
        let kept = request.body.len().min(room);
        self.request_bytes += kept;
        RequestRecord {
            index,
            at: now(),
            url: request.url.to_owned(),
            method: request.method.to_string(),
            headers: request.headers.clone(),
            auth: auth_info(request),
            body: request.body.slice(..kept),
            body_len: request.body.len(),
        }
    }

    /// A new attempt for `request` (upstream's `RecordAPIRequest` with the
    /// request log on, and its `deferAPIRequest` with it off).
    pub(crate) fn record_request(&mut self, request: &AttemptRequest<'_>) {
        let index = self.list.len() + 1;
        let record = self.record(index, request);
        self.list.push(Attempt::new(index, Some(record)));
    }

    /// The latest attempt of `list`, made when there is none (upstream's
    /// `ensureAttempt`), with its `=== API RESPONSE n ===` intro written
    /// (`ensureResponseIntro`).
    fn current<'a>(list: &'a mut Vec<Attempt>, budget: &mut Budget) -> Option<&'a mut Attempt> {
        if list.is_empty() {
            list.push(Attempt::new(1, None));
        }
        let (attempt, previous) = list.split_last_mut()?;
        if !attempt.intro_written {
            let padding = previous
                .iter()
                .rev()
                .find(|previous| previous.intro_written)
                .map_or(0, |previous| {
                    2usize.saturating_sub(previous.trailing_newlines)
                });
            attempt.write(budget, "\n".repeat(padding).as_bytes());
            attempt.write(
                budget,
                format!("=== API RESPONSE {} ===\n", attempt.index).as_bytes(),
            );
            attempt.write(budget, format!("Timestamp: {}\n", now()).as_bytes());
            attempt.write(budget, b"\n");
            attempt.intro_written = true;
        }
        Some(attempt)
    }

    /// The answer's status and headers (upstream's
    /// `RecordAPIResponseMetadata`).
    pub(crate) fn record_metadata(&mut self, status: u16, headers: &HeaderMap) {
        let budget = &mut self.responses;
        let Some(attempt) = Self::current(&mut self.list, budget) else {
            return;
        };
        if status > 0 && !attempt.status_written {
            attempt.write(budget, format!("Status: {status}\n").as_bytes());
            attempt.status_written = true;
        }
        if !attempt.headers_written {
            let mut text = String::from("Headers:\n");
            write_headers(&mut text, headers);
            attempt.write(budget, text.as_bytes());
            attempt.headers_written = true;
            attempt.write(budget, b"\n");
        }
    }

    /// The attempt's error (upstream's `RecordAPIResponseError`).
    pub(crate) fn record_error(&mut self, message: &str) {
        let budget = &mut self.responses;
        let Some(attempt) = Self::current(&mut self.list, budget) else {
            return;
        };
        if attempt.body_started && !attempt.body_has_content {
            attempt.body_started = false;
        }
        if attempt.error_written {
            attempt.write(budget, b"\n");
        }
        attempt.write(budget, format!("Error: {message}\n").as_bytes());
        attempt.error_written = true;
    }

    /// The next part of the answer's body (upstream's
    /// `AppendAPIResponseChunk`): trimmed, and kept apart from the part
    /// before by a blank line, or by a line break after an SSE `event:`
    /// line.
    pub(crate) fn append_chunk(&mut self, chunk: &[u8]) {
        let data = trim_space(chunk);
        if data.is_empty() {
            return;
        }
        let budget = &mut self.responses;
        let Some(attempt) = Self::current(&mut self.list, budget) else {
            return;
        };
        if !attempt.headers_written {
            attempt.write(budget, b"Headers:\n<none>\n");
            attempt.headers_written = true;
            attempt.write(budget, b"\n");
        }
        if !attempt.body_started {
            attempt.write(budget, b"Body:\n");
            attempt.body_started = true;
        }
        let is_event = data.starts_with(b"event:");
        let is_data = data.starts_with(b"data:");
        if attempt.body_has_content {
            let separator: &[u8] = if attempt.prev_was_sse_event && is_data {
                b"\n"
            } else {
                b"\n\n"
            };
            attempt.write(budget, separator);
        }
        attempt.write(budget, data);
        attempt.body_has_content = true;
        attempt.prev_was_sse_event = is_event;
    }

    /// Counts `dropped` bytes of an answer as left out.
    fn drop_response(&mut self, dropped: usize) {
        self.responses.dropped = self.responses.dropped.saturating_add(dropped);
    }

    /// A message sent on an upstream WebSocket (upstream's
    /// `RecordAPIWebsocketRequest`).
    pub(crate) fn ws_request(&mut self, request: &AttemptRequest<'_>) {
        let mut event = format!("Timestamp: {}\nEvent: api.websocket.request\n", now());
        if !request.url.is_empty() {
            event.push_str(&format!("Upstream URL: {}\n", mask_url(request.url)));
        }
        let auth = auth_info(request);
        if !auth.is_empty() {
            event.push_str(&format!("Auth: {auth}\n"));
        }
        event.push_str("Headers:\n");
        write_headers(&mut event, request.headers);
        event.push_str("\nBody:\n");
        let mut event = event.into_bytes();
        if request.body.is_empty() {
            event.extend_from_slice(b"<empty>");
        } else {
            event.extend_from_slice(request.body);
        }
        event.push(b'\n');
        self.append_timeline(&event);
    }

    /// A message read from an upstream WebSocket (upstream's
    /// `AppendAPIWebsocketResponse`).
    pub(crate) fn ws_response(&mut self, payload: &[u8]) {
        let data = trim_space(payload);
        if data.is_empty() {
            return;
        }
        let mut event =
            format!("Timestamp: {}\nEvent: api.websocket.response\n", now()).into_bytes();
        event.extend_from_slice(data);
        event.push(b'\n');
        self.append_timeline(&event);
    }

    /// An upstream WebSocket's error (upstream's `RecordAPIWebsocketError`,
    /// without a stage).
    pub(crate) fn ws_error(&mut self, message: &str) {
        let event = format!(
            "Timestamp: {}\nEvent: api.websocket.error\nError: {message}\n",
            now()
        );
        self.append_timeline(event.as_bytes());
    }

    /// Adds an event to the WebSocket timeline, a blank line after the one
    /// before (upstream's `appendAPIWebsocketTimeline`).
    fn append_timeline(&mut self, chunk: &[u8]) {
        let data = self.timeline_budget.take(trim_space(chunk));
        if data.is_empty() {
            return;
        }
        if !self.timeline.is_empty() {
            if !self.timeline.ends_with(b"\n") {
                self.timeline.push(b'\n');
            }
            self.timeline.push(b'\n');
        }
        self.timeline.extend_from_slice(data);
    }

    /// The upstream requests, each attempt's in turn (upstream's
    /// `API_REQUEST`).
    pub(crate) fn api_request(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for attempt in &self.list {
            match &attempt.request {
                Some(record) => record.render(&mut out),
                None => out.extend_from_slice(
                    format!("=== API REQUEST {} ===\n<missing>\n\n", attempt.index).as_bytes(),
                ),
            }
        }
        out
    }

    /// The upstream answers, each attempt's in turn, ending with a line
    /// break (upstream's `API_RESPONSE`).
    pub(crate) fn api_response(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for attempt in &self.list {
            out.extend_from_slice(&attempt.response);
        }
        if !out.is_empty() && !out.ends_with(b"\n") {
            out.push(b'\n');
        }
        out
    }

    /// The upstream WebSocket timeline.
    pub(crate) fn timeline(&self) -> &[u8] {
        &self.timeline
    }

    /// How many bytes of the answers and the timeline were left out.
    pub(crate) fn dropped(&self) -> usize {
        self.responses
            .dropped
            .saturating_add(self.timeline_budget.dropped)
    }
}

/// What the tap knows of the executor call it sees now.
#[derive(Default)]
struct Call {
    /// The kind of the call's latest attempt; `None` before its first.
    kind: Option<AttemptKind>,
    /// The status of the latest attempt's answer.
    status: Option<u16>,
    /// An answer's body, kept whole until the attempt ends.
    body: Vec<u8>,
    /// The part of a stream's line read so far.
    line: Vec<u8>,
    /// Whether the executor told the latest attempt's failure
    /// ([`Tap::attempt_error`]).
    told: bool,
}

impl Call {
    /// Whether the latest attempt's answer is read a line at a time: a
    /// successful stream.
    fn by_line(&self) -> bool {
        self.kind == Some(AttemptKind::Stream)
            && self
                .status
                .is_some_and(|status| (200..300).contains(&status))
    }

    /// Whether the latest attempt is recorded in `mode`: every attempt with
    /// the request log on, and with it off all but an upstream WebSocket's.
    fn recorded(&self, mode: Mode) -> bool {
        mode == Mode::Full || self.kind != Some(AttemptKind::Websocket)
    }

    /// Whether the body of the latest attempt's answer is recorded in
    /// `mode`: every body with the request log on, and with it off that of
    /// an answer with an error status.
    fn body_recorded(&self, mode: Mode) -> bool {
        mode == Mode::Full
            || (self.recorded(mode)
                && self
                    .status
                    .is_some_and(|status| !(200..300).contains(&status)))
    }
}

/// The tap that records a request's upstream attempts in its log.
pub(crate) struct RequestLogTap {
    context: Arc<RequestContext>,
    call: Mutex<Call>,
}

impl RequestLogTap {
    pub(crate) fn new(context: Arc<RequestContext>) -> Self {
        Self {
            context,
            call: Mutex::new(Call::default()),
        }
    }

    fn call(&self) -> std::sync::MutexGuard<'_, Call> {
        self.call.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `f` on the request's capture while it is open.
    fn with(&self, f: impl FnOnce(&mut Capture)) {
        self.context.request_log().with(f);
    }

    /// Writes what the call holds of the latest answer's body.
    fn flush(&self, call: &mut Call) {
        let line = std::mem::take(&mut call.line);
        let body = std::mem::take(&mut call.body);
        if line.is_empty() && body.is_empty() {
            return;
        }
        self.with(|capture| {
            capture.attempts.append_chunk(&line);
            capture.attempts.append_chunk(&body);
        });
    }
}

impl Tap for RequestLogTap {
    fn attempt_request(&self, request: &AttemptRequest<'_>) {
        let mut call = self.call();
        self.flush(&mut call);
        call.kind = Some(request.kind);
        call.status = None;
        call.told = false;
        self.with(|capture| {
            capture.add_secrets(request.secrets);
            match (capture.mode, request.kind) {
                (Mode::Full, AttemptKind::Websocket) => capture.attempts.ws_request(request),
                (Mode::ErrorsOnly, AttemptKind::Websocket) => {}
                _ => capture.attempts.record_request(request),
            }
        });
    }

    fn response_head(&self, status: u16, headers: &HeaderMap) {
        let mut call = self.call();
        self.flush(&mut call);
        call.status = Some(status);
        self.with(|capture| {
            capture.add_header_secrets(headers);
            if call.recorded(capture.mode) {
                capture.attempts.record_metadata(status, headers);
            }
        });
    }

    fn chunk(&self, chunk: &Bytes) {
        let mut call = self.call();
        if call.kind == Some(AttemptKind::Websocket) {
            drop(call);
            self.with(|capture| {
                if capture.mode == Mode::Full {
                    capture.attempts.ws_response(chunk);
                }
            });
            return;
        }
        let Some(mode) = self.context.request_log().mode() else {
            return;
        };
        if !call.body_recorded(mode) {
            return;
        }
        if !call.by_line() {
            let room = CAPTURE_LIMIT.saturating_sub(call.body.len());
            let kept = chunk.get(..room.min(chunk.len())).unwrap_or_default();
            call.body.extend_from_slice(kept);
            if kept.len() < chunk.len() {
                let dropped = chunk.len() - kept.len();
                self.with(|capture| capture.attempts.drop_response(dropped));
            }
            return;
        }
        call.line.extend_from_slice(chunk);
        let mut lines = Vec::new();
        while let Some(end) = call.line.iter().position(|&b| b == b'\n') {
            let rest = call.line.split_off(end + 1);
            lines.push(std::mem::replace(&mut call.line, rest));
        }
        if call.line.len() > CAPTURE_LIMIT {
            lines.push(std::mem::take(&mut call.line));
        }
        drop(call);
        if lines.is_empty() {
            return;
        }
        self.with(|capture| {
            for line in &lines {
                capture.attempts.append_chunk(line);
            }
        });
    }

    fn attempt_error(&self, message: &str) {
        let mut call = self.call();
        self.flush(&mut call);
        call.told = true;
        self.with(|capture| {
            if call.recorded(capture.mode) {
                capture.attempts.record_error(message);
            }
        });
    }

    fn error(&self, error: &ExecError) {
        let mut call = self.call();
        self.flush(&mut call);
        let kind = call.kind;
        let answered = call.status.is_some();
        let told = call.told;
        drop(call);
        let message = error.to_string();
        self.with(|capture| {
            match kind {
                // An error upstream records as the attempt's
                // (`RecordAPIResponseError`) isn't on the timeline, which
                // is kept only with the request log on.
                Some(AttemptKind::Websocket) if told || capture.mode != Mode::Full => {}
                Some(AttemptKind::Websocket) => capture.attempts.ws_error(&message),
                // A failure after the head was told through
                // `attempt_error` where upstream records one.
                Some(_) if !answered => capture.attempts.record_error(&message),
                _ => {}
            }
        });
    }

    fn finish(&self, _outcome: Outcome) {
        let mut call = self.call();
        self.flush(&mut call);
        *call = Call::default();
    }
}

impl Drop for RequestLogTap {
    fn drop(&mut self) {
        let mut call = std::mem::take(self.call.get_mut().unwrap_or_else(PoisonError::into_inner));
        self.flush(&mut call);
    }
}

#[cfg(test)]
mod tests {
    use http::{HeaderValue, Method};

    use super::*;
    use crate::auth::Auth;
    use crate::exec::Format;

    static NO_SECRETS: redact::Secrets = redact::Secrets::new();

    fn request<'a>(
        url: &'a str,
        method: &'a Method,
        headers: &'a HeaderMap,
        body: &'a Bytes,
        auth: &'a Auth,
        format: &'a Format,
    ) -> AttemptRequest<'a> {
        AttemptRequest {
            kind: AttemptKind::Execute,
            method,
            url,
            headers,
            body,
            provider: "codex",
            model: "gpt-5",
            format,
            auth,
            secrets: &NO_SECRETS,
        }
    }

    // Not upstream's: the user info and key-like parameters of an upstream
    // URL are masked.
    #[test]
    fn masks_upstream_urls() {
        assert_eq!(
            mask_url("https://user:pass@example.com:8443/v1?key=AIzaSyA-123456789&alt=sse#x"),
            "https://[redacted]@example.com:8443/v1?key=AIza...6789&alt=sse#x"
        );
        assert_eq!(
            mask_url("https://example.com/a@b?token=abcdefghijkl"),
            "https://example.com/a@b?token=abcd...ijkl"
        );
        assert_eq!(mask_url("wss://example.com/ws"), "wss://example.com/ws");
        assert_eq!(mask_url(""), "");
    }

    // Not upstream's: the request block, as upstream's
    // newAPIRequestLogBuilder writes it.
    #[test]
    fn writes_request_blocks() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer sk-abcdefghijkl"),
        );
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        let mut auth = Auth {
            id: "codex-a.json".to_owned(),
            label: "Team A".to_owned(),
            ..Auth::default()
        };
        auth.attributes
            .insert("auth_kind".to_owned(), "api-key".to_owned());
        auth.attributes
            .insert("api_key".to_owned(), "sk-abcdefghijkl".to_owned());
        let body = Bytes::from_static(b"{\"model\":\"gpt-5\"}");
        let format = Format::from("codex");
        let method = Method::POST;
        let mut attempts = Attempts::default();
        attempts.record_request(&request(
            "https://api.example.com/v1/responses",
            &method,
            &headers,
            &body,
            &auth,
            &format,
        ));
        let text = String::from_utf8(attempts.api_request()).unwrap();
        let (first, rest) = text.split_once('\n').unwrap();
        assert_eq!(first, "=== API REQUEST 1 ===");
        let rest = rest.split_once('\n').unwrap().1;
        assert_eq!(
            rest,
            "Upstream URL: https://api.example.com/v1/responses\nHTTP Method: POST\nAuth: provider=codex, auth_id=codex-a.json, label=Team A, type=api_key value=sk-a...ijkl\n\nHeaders:\nAuthorization: Bearer sk-a...ijkl\nContent-Type: application/json\n\nBody:\n{\"model\":\"gpt-5\"}\n\n"
        );
    }

    // Not upstream's: an answer that comes before any request is numbered
    // as upstream numbers it, after a `<missing>` request.
    #[test]
    fn records_answers_without_requests() {
        let mut attempts = Attempts::default();
        attempts.append_chunk(b"event: a");
        attempts.append_chunk(b"data: 1");
        attempts.append_chunk(b"  ");
        attempts.append_chunk(b"data: 2\n");
        assert_eq!(
            String::from_utf8(attempts.api_request()).unwrap(),
            "=== API REQUEST 1 ===\n<missing>\n\n"
        );
        let response = String::from_utf8(attempts.api_response()).unwrap();
        let body = response.split_once("\n\n").unwrap().1;
        assert_eq!(
            body,
            "Headers:\n<none>\n\nBody:\nevent: a\ndata: 1\n\ndata: 2\n"
        );
    }

    // Not upstream's: the WebSocket timeline's events, a blank line apart.
    #[test]
    fn records_the_websocket_timeline() {
        let headers = HeaderMap::new();
        let body = Bytes::from_static(b"{\"type\":\"response.create\"}");
        let auth = Auth::default();
        let format = Format::from("codex");
        let method = Method::GET;
        let mut attempts = Attempts::default();
        let mut sent = request(
            "wss://example.com/ws",
            &method,
            &headers,
            &body,
            &auth,
            &format,
        );
        sent.kind = AttemptKind::Websocket;
        attempts.ws_request(&sent);
        attempts.ws_response(b" {\"type\":\"response.completed\"} ");
        attempts.ws_error("closed");
        let timeline = String::from_utf8(attempts.timeline().to_vec()).unwrap();
        let events: Vec<_> = timeline.split("\n\nTimestamp: ").collect();
        assert_eq!(events.len(), 3, "{timeline}");
        assert!(events[0].contains(
            "\nEvent: api.websocket.request\nUpstream URL: wss://example.com/ws\nAuth: provider=codex\nHeaders:\n<none>\n\nBody:\n{\"type\":\"response.create\"}"
        ));
        assert!(
            events[1]
                .ends_with("\nEvent: api.websocket.response\n{\"type\":\"response.completed\"}")
        );
        assert!(events[2].ends_with("\nEvent: api.websocket.error\nError: closed"));
        assert!(attempts.api_request().is_empty());
    }
}
