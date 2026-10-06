// Ported from CLIProxyAPI internal/logging/request_logger_writer.go
// (logRequestWithSources, ensureLogsDir, cleanupOldErrorLogs) and
// internal/api/middleware/response_writer.go (Finalize, extractRequestBody)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The writer thread: it renders each finished request's log, scrubs it
//! and writes it, then trims the error logs.
//!
//! Requests hand their logs over through a bounded queue, never waiting:
//! a log that finds the queue full is dropped and counted, and the drops
//! are warned about at most once a minute. The thread starts with the
//! first log.
//!
//! Deviations from upstream:
//! - Logs are written on a thread of their own, after the answer is sent;
//!   upstream writes a log on the request's goroutine, and a stream's as it
//!   goes.
//! - Every secret the request's attempts sent, the secrets of the client's
//!   credential headers, cookies and URL and of the answers' headers, and
//!   the client's key, are scrubbed from the whole log however short they
//!   are, and from the path its name is made from (see [`Policy::Disk`]).
//! - What the bodies had past the capture limits is counted in one line at
//!   the end of the log.
//! - Removing an old error log is tried three times, for Windows.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Local};
use http::header::{CONTENT_ENCODING, CONTENT_TYPE};

use super::attempts::Attempts;
use super::body_source::CAPTURE_LIMIT;
use super::format::{self, DECODE_LIMIT, Sections};
use super::names::{create_unique_log_file, error_filename, filename};
use super::{Answer, ApiError, Downstream, RequestBody};
use crate::observe::redact::{Policy, Secrets};

/// How many finished logs may wait for the writer.
const QUEUE: usize = 1024;

/// How often dropped logs are warned about.
const WARN_EVERY: Duration = Duration::from_secs(60);

/// How many times removing an old error log is tried.
const REMOVE_TRIES: usize = 3;

/// A finished request's log, as the writer gets it.
pub(crate) struct Entry {
    /// The directory to write it to.
    pub dir: PathBuf,
    /// Whether it is a forced error log (`error-*.log`).
    pub forced: bool,
    /// How many error logs to keep; none are removed when 0 or less.
    pub max_error_files: i64,
    /// Whether the request was logged whole.
    pub full: bool,
    /// The request's ID.
    pub request_id: String,
    /// When the request arrived.
    pub arrived_at: DateTime<Local>,
    /// The client's request.
    pub downstream: Downstream,
    /// The answer.
    pub answer: Answer,
    /// The upstream attempts.
    pub attempts: Attempts,
    /// The handlers' errors.
    pub api_errors: Vec<ApiError>,
    /// What to scrub from the log, besides the secrets of the client's
    /// request and the answer, which are gathered when it is written.
    pub secrets: Secrets,
}

enum Job {
    Write(Box<Entry>),
    Flush(mpsc::Sender<()>),
}

/// The handle to the writer thread.
#[derive(Default)]
pub(crate) struct Writer {
    sender: OnceLock<Option<SyncSender<Job>>>,
    dropped: AtomicU64,
    last_warned: Mutex<Option<Instant>>,
}

impl Writer {
    /// The queue to the thread, which starts with the first call.
    fn sender(&self) -> Option<&SyncSender<Job>> {
        self.sender
            .get_or_init(|| {
                let (sender, receiver) = mpsc::sync_channel(QUEUE);
                match thread::Builder::new()
                    .name("request-log".to_owned())
                    .spawn(move || run(&receiver))
                {
                    Ok(_) => Some(sender),
                    Err(error) => {
                        tracing::warn!(error = %error, "failed to start the request log writer");
                        None
                    }
                }
            })
            .as_ref()
    }

    /// Hands `entry` to the thread, or drops it when the queue is full.
    pub(crate) fn submit(&self, entry: Entry) {
        let sent = match self.sender() {
            Some(sender) => match sender.try_send(Job::Write(Box::new(entry))) {
                Ok(()) => true,
                Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
            },
            None => false,
        };
        if !sent {
            self.note_drop();
        }
    }

    /// Counts a dropped log, and warns at most once a minute.
    fn note_drop(&self) {
        let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        let mut last = self
            .last_warned
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        if last.is_none_or(|at| now.duration_since(at) >= WARN_EVERY) {
            *last = Some(now);
            tracing::warn!(dropped, "request log queue is full; logs were dropped");
        }
    }

    /// Waits until the thread has written every log handed to it so far.
    pub(crate) fn flush(&self) {
        let Some(sender) = self.sender.get().and_then(Option::as_ref) else {
            return;
        };
        let (done, wait) = mpsc::channel();
        if sender.send(Job::Flush(done)).is_ok() {
            let _ = wait.recv();
        }
    }
}

/// The writer thread's loop; it ends when the logger is dropped.
fn run(receiver: &Receiver<Job>) {
    for job in receiver {
        match job {
            Job::Write(entry) => write(&entry),
            Job::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

/// Writes `entry`'s log, and trims the error logs after a forced one.
fn write(entry: &Entry) {
    let (name, content) = render(entry, Local::now());
    if let Err(error) = write_file(&entry.dir, &name, &content) {
        tracing::warn!(error = %error, "failed to write request log");
        return;
    }
    if entry.forced
        && let Err(error) = cleanup_old_error_logs(&entry.dir, entry.max_error_files)
    {
        tracing::warn!(error = %error, "failed to clean up old error logs");
    }
}

/// Makes the log file `name` in `dir`, or the next free name, and writes
/// `content` to it.
fn write_file(dir: &Path, name: &str, content: &[u8]) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let (mut file, _) = create_unique_log_file(dir, name)?;
    file.write_all(content)?;
    file.flush()
}

/// The name and the content of `entry`'s log, written at `now`.
pub(crate) fn render(entry: &Entry, now: DateTime<Local>) -> (String, Vec<u8>) {
    let downstream = &entry.downstream;
    let answer = &entry.answer;
    let body = request_body(downstream);

    let content_type = answer
        .headers
        .get(CONTENT_TYPE)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned());
    let streaming = entry.full && format::is_streaming(content_type.as_deref(), &body);

    let api_request = entry.attempts.api_request();
    let api_response = entry.attempts.api_response();
    let response = answer.body.concat();

    let (info_time, name_time) = if streaming {
        (answer.head_at, answer.head_at)
    } else {
        (entry.arrived_at, now)
    };
    let sections = Sections {
        url: &downstream.url,
        method: &downstream.method,
        headers: &downstream.headers,
        body: &body,
        timestamp: format::rfc3339_nano(&info_time),
        api_ws_timeline: entry.attempts.timeline(),
        api_request: &api_request,
        api_errors: &entry.api_errors,
        api_response: &api_response,
        status: answer.status,
        response_headers: &answer.headers,
        response: &response,
    };
    let mut content = if streaming {
        format::streaming(&sections)
    } else {
        format::non_streaming(&sections)
    };

    let dropped = entry
        .attempts
        .dropped()
        .saturating_add(answer.body.dropped());
    if dropped > 0 {
        if !content.is_empty() && !content.ends_with(b"\n") {
            content.push(b'\n');
        }
        content.extend_from_slice(
            format!(
                "[REQUEST LOG TRUNCATED: {dropped} bytes past the capture limits were left out]\n"
            )
            .as_bytes(),
        );
    }

    let secrets = log_secrets(entry);
    if let std::borrow::Cow::Owned(scrubbed) = secrets.bytes(&content, Policy::Disk) {
        content = scrubbed;
    }

    let url = secrets.str(&downstream.url, Policy::Disk);
    let name = if entry.forced {
        error_filename(&url, &entry.request_id, name_time)
    } else {
        filename(&url, &entry.request_id, name_time)
    };
    (name, content)
}

/// Every secret to scrub from `entry`'s log: those its attempts sent, the
/// client's key and what else it kept, and those of the client's request
/// and of the answer, in their headers and the request's URL.
fn log_secrets(entry: &Entry) -> Secrets {
    let mut secrets = entry.secrets.clone();
    secrets.add_headers(&entry.downstream.headers);
    secrets.extend(&entry.downstream.secrets);
    secrets.add_headers(&entry.answer.headers);
    secrets
}

/// The client's body as the log shows it: decoded as its first
/// `Content-Encoding` says, with markers saying what is missing (upstream's
/// `captureRequestInfo` and `extractRequestBody`).
fn request_body(downstream: &Downstream) -> Vec<u8> {
    let encoding = downstream
        .headers
        .get(CONTENT_ENCODING)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .unwrap_or_default();
    match &downstream.body {
        RequestBody::None => Vec::new(),
        RequestBody::Captured { raw, truncated } => {
            let mut body = format::decode_request_body(raw, &encoding, DECODE_LIMIT).into_owned();
            if *truncated {
                push_marker(
                    &mut body,
                    &format!(
                        "[REQUEST BODY TRUNCATED: captured first {} bytes]",
                        raw.len()
                    ),
                );
            }
            body
        }
        RequestBody::Deferred(capture) => {
            let (raw, marker) = capture.snapshot();
            let mut body = format::decode_request_body(&raw, &encoding, CAPTURE_LIMIT).into_owned();
            if !marker.is_empty() {
                push_marker(&mut body, &marker);
            }
            body
        }
    }
}

/// Appends `marker` to `body` on a line of its own.
fn push_marker(body: &mut Vec<u8>, marker: &str) {
    if !body.is_empty() && !body.ends_with(b"\n") {
        body.push(b'\n');
    }
    body.extend_from_slice(marker.as_bytes());
}

/// Removes the oldest `error-*.log` files in `dir` beyond `max` (upstream's
/// `cleanupOldErrorLogs`); none when `max` is 0 or less.
pub(crate) fn cleanup_old_error_logs(dir: &Path, max: i64) -> io::Result<()> {
    let Ok(max) = usize::try_from(max) else {
        return Ok(());
    };
    if max == 0 {
        return Ok(());
    }
    let mut files: Vec<(String, SystemTime)> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !super::is_error_log_name(&name) {
            continue;
        }
        match entry.metadata().and_then(|meta| meta.modified()) {
            Ok(modified) => files.push((name, modified)),
            Err(error) => tracing::warn!(error = %error, "failed to read error log info"),
        }
    }
    if files.len() <= max {
        return Ok(());
    }
    files.sort_by_key(|file| std::cmp::Reverse(file.1));
    for (name, _) in files.iter().skip(max) {
        if let Err(error) = remove_with_retry(&dir.join(name)) {
            tracing::warn!(error = %error, "failed to remove old error log: {name}");
        }
    }
    Ok(())
}

/// Removes `path`, trying again when Windows has it open.
fn remove_with_retry(path: &Path) -> io::Result<()> {
    let mut last = Ok(());
    for attempt in 0..REMOVE_TRIES {
        match fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => last = Err(error),
        }
        if attempt + 1 < REMOVE_TRIES {
            thread::sleep(Duration::from_millis(50));
        }
    }
    last
}
