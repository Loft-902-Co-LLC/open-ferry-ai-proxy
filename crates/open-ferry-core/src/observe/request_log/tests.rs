//! Tests of the request log, end to end from a request's taps to the file
//! on disk. The ports of CLIProxyAPI
//! internal/logging/request_logger_collision_test.go and
//! internal/runtime/executor/helps/logging_helpers_test.go (v8.0.10, MIT)
//! are in `tests/`; the middleware's tests are in open-ferry-server's
//! `request_log/tests`.
//!
//! The tests here are not upstream's: upstream tests its logger through the
//! middleware.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use chrono::{Local, TimeZone};
use http::{HeaderMap, HeaderValue, Method};

use super::*;
use crate::auth::Auth;
use crate::config::Config;
use crate::exec::{ExecError, Format};
use crate::observe::{AttemptKind, AttemptRequest, Outcome};

mod logging_helpers;
mod request_logger_collision;

const SECRET: &str = "sk-upstream-secret-123456";
const CLIENT_KEY: &str = "client-key-abcdefgh";

fn config(request_log: bool) -> Config {
    Config {
        request_log,
        error_logs_max_files: 10,
        ..Config::default()
    }
}

fn logger(dir: &Path, request_log: bool) -> RequestLogger {
    RequestLogger::new(&config(request_log), dir, Path::new(""))
}

fn context(path: &str) -> Arc<RequestContext> {
    let context = RequestContext::new(Method::POST, path.to_owned());
    context.set_client_key(CLIENT_KEY);
    Arc::new(context)
}

fn downstream(path: &str, body: &'static [u8]) -> Downstream {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert(
        "authorization",
        HeaderValue::from_static("Bearer client-key-abcdefgh"),
    );
    Downstream {
        url: Downstream::url(path, Some("key=AIzaSyA-1234567890&alt=sse")),
        method: "POST".to_owned(),
        headers,
        body: RequestBody::Captured {
            raw: Bytes::from_static(body),
            truncated: false,
        },
    }
}

fn answer(status: u16, content_type: &'static str, body: &'static [u8]) -> Answer {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static(content_type));
    let mut capture = ResponseCapture::default();
    capture.push(&Bytes::from_static(body));
    Answer {
        status,
        headers,
        body: capture,
        head_at: Local::now(),
        canceled: false,
    }
}

/// Runs one upstream attempt through `tap`: a request carrying [`SECRET`],
/// then an answer with `status` and `body`.
fn attempt(tap: &Arc<dyn Tap>, kind: AttemptKind, status: u16, body: &'static [u8]) {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {SECRET}")).unwrap(),
    );
    let auth = Auth {
        id: "codex-a.json".to_owned(),
        provider: "codex".to_owned(),
        ..Auth::default()
    };
    let request_body = Bytes::from(format!("{{\"model\":\"gpt-5\",\"echo\":\"{SECRET}\"}}"));
    let format = Format::from("codex");
    tap.attempt_request(&AttemptRequest {
        kind,
        method: &Method::POST,
        url: "https://user:pass@api.example.com/v1/responses?api_key=abcdefghijkl",
        headers: &headers,
        body: &request_body,
        provider: "codex",
        model: "gpt-5",
        format: &format,
        auth: &auth,
        secrets: &[SECRET],
    });
    let mut response_headers = HeaderMap::new();
    response_headers.insert("content-type", HeaderValue::from_static("application/json"));
    tap.response_head(status, &response_headers);
    tap.chunk(&Bytes::from_static(body));
    tap.finish(if status < 400 {
        Outcome::Completed
    } else {
        Outcome::Failed
    });
}

/// The names and contents of the files in `dir`, sorted by name.
fn files(dir: &Path) -> Vec<(String, String)> {
    let mut found: Vec<_> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|entry| {
                    let entry = entry.unwrap();
                    (
                        entry.file_name().to_string_lossy().into_owned(),
                        fs::read_to_string(entry.path()).unwrap(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

// Not upstream's: with `request-log` on, a request's log has its request,
// its upstream attempt and its answer, with every credential masked or
// scrubbed.
#[test]
fn writes_full_logs() {
    let dir = tempfile::tempdir().unwrap();
    let logger = logger(dir.path(), true);
    let context = context("/v1/chat/completions");
    assert_eq!(logger.start(&context), Some(Mode::Full));
    let tap = logger.tap(&context).unwrap();
    attempt(&tap, AttemptKind::Execute, 200, b"{\"ok\":true}");
    finish(
        &context,
        downstream("/v1/chat/completions", b"{\"messages\":[]}"),
        answer(200, "application/json", b"{\"id\":\"1\"}"),
    );
    logger.flush();

    let files = files(dir.path());
    assert_eq!(files.len(), 1, "{files:?}");
    let (name, log) = &files[0];
    assert!(name.starts_with("v1-chat-completions-"), "{name}");
    assert!(
        name.ends_with(&format!("-{}.log", context.id.short())),
        "{name}"
    );
    assert!(log.starts_with("=== REQUEST INFO ===\nVersion: "));
    assert!(
        log.contains("URL: /v1/chat/completions?key=AIza...7890&alt=sse\n"),
        "{log}"
    );
    assert!(log.contains("Authorization: Bearer clie...efgh\n"), "{log}");
    assert!(
        log.contains("=== REQUEST BODY ===\n{\"messages\":[]}\n\n"),
        "{log}"
    );
    assert!(log.contains("=== API REQUEST 1 ===\n"), "{log}");
    assert!(
        log.contains(
            "Upstream URL: https://[redacted]@api.example.com/v1/responses?api_key=abcd...ijkl\n"
        ),
        "{log}"
    );
    assert!(log.contains("\"echo\":\"[redacted]\""), "{log}");
    assert!(log.contains("=== API RESPONSE 1 ===\n"), "{log}");
    assert!(log.contains("Body:\n{\"ok\":true}\n"), "{log}");
    assert!(
        log.ends_with(
            "=== RESPONSE ===\nStatus: 200\nContent-Type: application/json\n\n{\"id\":\"1\"}\n"
        ),
        "{log}"
    );
    assert!(!log.contains(SECRET), "{log}");
    assert!(!log.contains(CLIENT_KEY), "{log}");
    assert!(!log.contains("user:pass"), "{log}");
}

// Not upstream's: with `request-log` off, a request that went well leaves
// no file, and one that failed leaves an error log with the upstream
// request the attempt sent.
#[test]
fn writes_error_logs_only_on_errors() {
    let dir = tempfile::tempdir().unwrap();
    let logger = logger(dir.path(), false);

    let ok = context("/v1/chat/completions");
    assert_eq!(logger.start(&ok), Some(Mode::ErrorsOnly));
    attempt(&logger.tap(&ok).unwrap(), AttemptKind::Execute, 200, b"{}");
    finish(
        &ok,
        downstream("/v1/chat/completions", b"{}"),
        answer(200, "application/json", b"{}"),
    );

    let failed = context("/v1/chat/completions");
    assert_eq!(logger.start(&failed), Some(Mode::ErrorsOnly));
    attempt(
        &logger.tap(&failed).unwrap(),
        AttemptKind::Execute,
        500,
        b"{\"error\":1}",
    );
    finish(
        &failed,
        downstream("/v1/chat/completions", b"{\"a\":1}"),
        answer(502, "application/json", b"{\"error\":\"bad gateway\"}"),
    );
    logger.flush();

    let files = files(dir.path());
    assert_eq!(files.len(), 1, "{files:?}");
    let (name, log) = &files[0];
    assert!(name.starts_with("error-v1-chat-completions-"), "{name}");
    assert!(log.contains("=== API REQUEST 1 ===\n"), "{log}");
    assert!(!log.contains("=== API RESPONSE"), "{log}");
    assert!(log.contains("=== RESPONSE ===\nStatus: 502\n"), "{log}");
    assert!(!log.contains(SECRET), "{log}");
}

// Not upstream's: a request the client left, or one with a client error
// only, isn't an error log; an API error that isn't a cancellation is.
#[test]
fn decides_what_is_actionable() {
    assert!(!has_actionable_error(200, false, &[]));
    assert!(has_actionable_error(500, false, &[]));
    assert!(!has_actionable_error(499, false, &[]));
    assert!(!has_actionable_error(200, true, &[]));
    assert!(has_actionable_error(500, true, &[]));
    let canceled = ApiError {
        status: 500,
        message: "Context Canceled".to_owned(),
        canceled: false,
    };
    assert!(!has_actionable_error(
        200,
        false,
        std::slice::from_ref(&canceled)
    ));
    let real = ApiError {
        status: 500,
        message: "upstream failed".to_owned(),
        canceled: false,
    };
    assert!(has_actionable_error(499, true, &[canceled, real]));
}

// Not upstream's: commercial mode turns capture off, read live.
#[test]
fn commercial_mode_disables_capture() {
    let dir = tempfile::tempdir().unwrap();
    let logger = logger(dir.path(), true);
    let mut commercial = config(true);
    commercial.commercial_mode = true;
    reconfigure(&logger, None, &commercial);
    let context = context("/v1/models");
    assert_eq!(logger.start(&context), None);
    assert!(logger.tap(&context).is_none());

    reconfigure(&logger, Some(&commercial), &config(true));
    assert_eq!(logger.start(&context), Some(Mode::Full));
    reconfigure(&logger, None, &commercial);
    assert!(logger.tap(&context).is_none());
    finish(
        &context,
        downstream("/v1/models", b""),
        answer(200, "application/json", b"{}"),
    );
    logger.flush();
    assert!(files(dir.path()).is_empty());
}

// Not upstream's: a logger that logs nothing gives no mode and no tap.
#[test]
fn the_default_logger_is_inert() {
    let logger = RequestLogger::default();
    let context = context("/v1/models");
    assert_eq!(logger.start(&context), None);
    assert!(logger.tap(&context).is_none());
    assert!(!logger.is_enabled());
    assert!(logger.dir().is_none());
    logger.flush();
}

// Ports NewFileRequestLogger's directory rule: a relative log directory is
// taken from the config file's directory.
#[test]
fn takes_relative_dirs_from_the_config_dir() {
    let config = config(false);
    let logger = RequestLogger::new(&config, Path::new("logs"), Path::new("conf/config.yaml"));
    assert_eq!(logger.dir(), Some(Path::new("conf/logs")));
    let logger = RequestLogger::new(&config, Path::new("logs"), Path::new("config.yaml"));
    assert_eq!(logger.dir(), Some(Path::new("logs")));
    let absolute = std::env::temp_dir().join("logs");
    let logger = RequestLogger::new(&config, &absolute, Path::new("conf/config.yaml"));
    assert_eq!(logger.dir(), Some(absolute.as_path()));
}

// Not upstream's: the oldest error logs beyond the limit are removed, and
// nothing else is.
#[test]
fn cleans_up_old_error_logs() {
    let dir = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    for (index, name) in ["error-a.log", "error-b.log", "error-c.log", "v1-x.log"]
        .iter()
        .enumerate()
    {
        let file = fs::File::create(dir.path().join(name)).unwrap();
        file.set_modified(now - Duration::from_secs(60 * (index as u64 + 1)))
            .unwrap();
    }
    fs::create_dir(dir.path().join("error-dir.log")).unwrap();

    writer::cleanup_old_error_logs(dir.path(), 0).unwrap();
    assert_eq!(files_named(dir.path()).len(), 5);
    writer::cleanup_old_error_logs(dir.path(), 2).unwrap();
    assert_eq!(
        files_named(dir.path()),
        ["error-a.log", "error-b.log", "error-dir.log", "v1-x.log"]
    );
}

fn files_named(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

// Not upstream's: a streamed answer's log is named for and stamped with
// the time its head was sent, and shows the upstream lines as they came.
#[test]
fn renders_streaming_logs() {
    let dir = tempfile::tempdir().unwrap();
    let logger = logger(dir.path(), true);
    let context = context("/v1/responses");
    logger.start(&context);
    let tap = logger.tap(&context).unwrap();
    attempt(&tap, AttemptKind::Stream, 200, b"data: 1\n\ndata: 2\n");
    let capture = context.request_log().lock().take().unwrap();
    let head_at = Local.with_ymd_and_hms(2026, 9, 23, 12, 0, 0).unwrap();
    let mut answer = answer(200, "text/event-stream", b"data: a\n\n");
    answer.head_at = head_at;
    let entry = writer::Entry {
        dir: dir.path().to_path_buf(),
        forced: false,
        max_error_files: 10,
        full: true,
        request_id: capture.request_id.clone(),
        arrived_at: capture.arrived_at,
        downstream: downstream("/v1/responses", b"{\"stream\":true}"),
        answer,
        attempts: capture.attempts,
        api_errors: vec![ApiError {
            status: 500,
            message: "left out of streams".to_owned(),
            canceled: false,
        }],
        secrets: vec![SECRET.to_owned()],
    };
    let (name, content) = writer::render(&entry, Local::now());
    let log = String::from_utf8(content).unwrap();
    assert!(
        name.starts_with("v1-responses-2026-09-23T120000-"),
        "{name}"
    );
    assert!(log.contains(&format!("Timestamp: {}\n", format::rfc3339_nano(&head_at))));
    assert!(log.contains("Body:\ndata: 1\n\ndata: 2\n"), "{log}");
    assert!(!log.contains("API ERROR"), "{log}");
    assert!(
        log.ends_with(
            "=== RESPONSE ===\nStatus: 200\nContent-Type: text/event-stream\n\ndata: a\n\n"
        ),
        "{log:?}"
    );
}

// Not upstream's: what the bodies had past the capture limits is counted
// in one line at the end of the log.
#[test]
fn notes_what_was_left_out() {
    let mut big = ResponseCapture::default();
    big.push(&Bytes::from(vec![b'x'; CAPTURE_LIMIT]));
    big.push(&Bytes::from_static(b"more"));
    let entry = writer::Entry {
        dir: std::env::temp_dir(),
        forced: false,
        max_error_files: 0,
        full: true,
        request_id: "req-1".to_owned(),
        arrived_at: Local::now(),
        downstream: downstream("/v1/x", b""),
        answer: Answer {
            body: big,
            ..answer(200, "text/plain", b"")
        },
        attempts: attempts::Attempts::default(),
        api_errors: Vec::new(),
        secrets: Vec::new(),
    };
    let (_, content) = writer::render(&entry, Local::now());
    let tail = content.get(content.len() - 120..).unwrap();
    let tail = String::from_utf8_lossy(tail);
    assert!(
        tail.ends_with(
            "x\n[REQUEST LOG TRUNCATED: 4 bytes past the capture limits were left out]\n"
        ),
        "{tail}"
    );
    assert_eq!(
        content
            .windows(b"[REQUEST LOG TRUNCATED".len())
            .filter(|window| *window == b"[REQUEST LOG TRUNCATED")
            .count(),
        1
    );
}

// Not upstream's: the body a handler read is shown with markers saying
// what it didn't read.
#[test]
fn shows_deferred_bodies_with_their_markers() {
    let capture = DeferredCapture::new(Some(20));
    capture.record(b"{\"partial\":");
    let mut downstream = downstream("/v1/x", b"");
    downstream.body = RequestBody::Deferred(capture);
    let entry = writer::Entry {
        dir: std::env::temp_dir(),
        forced: true,
        max_error_files: 0,
        full: false,
        request_id: "req-1".to_owned(),
        arrived_at: Local::now(),
        downstream,
        answer: answer(500, "application/json", b"{}"),
        attempts: attempts::Attempts::default(),
        api_errors: Vec::new(),
        secrets: Vec::new(),
    };
    let (name, content) = writer::render(&entry, Local::now());
    assert!(name.starts_with("error-v1-x-"), "{name}");
    let log = String::from_utf8(content).unwrap();
    assert!(
        log.contains(
            "=== REQUEST BODY ===\n{\"partial\":\n[REQUEST BODY CAPTURE INCOMPLETE: consumed 11 of 20 bytes]\n\n"
        ),
        "{log}"
    );
}

// Not upstream's: an error a handler records is shown in Full mode only,
// and a WebSocket session's log is written when its context goes.
#[test]
fn records_api_errors_and_finishes_later() {
    let dir = tempfile::tempdir().unwrap();
    let logger = logger(dir.path(), true);
    let context = context("/v1/responses");
    logger.start(&context);
    context
        .request_log()
        .record_api_error(502, "upstream went away", false);
    let tap = logger.tap(&context).unwrap();
    tap.attempt_request(&AttemptRequest {
        kind: AttemptKind::Execute,
        method: &Method::POST,
        url: "https://api.example.com/v1/responses",
        headers: &HeaderMap::new(),
        body: &Bytes::new(),
        provider: "codex",
        model: "gpt-5",
        format: &Format::from("codex"),
        auth: &Auth::default(),
        secrets: &[],
    });
    tap.error(&ExecError::canceled());
    drop(tap);
    finish_later(
        &context,
        downstream("/v1/responses", b""),
        answer(101, "", b""),
    );
    logger.flush();
    assert!(files(dir.path()).is_empty());
    drop(context);
    logger.flush();
    let files = files(dir.path());
    assert_eq!(files.len(), 1, "{files:?}");
    let log = &files[0].1;
    assert!(
        log.contains("=== API ERROR RESPONSE ===\nHTTP Status: 502\nupstream went away\n"),
        "{log}"
    );
    assert!(log.contains("Error: context canceled\n"), "{log}");
    assert!(log.contains("=== RESPONSE ===\nStatus: 101\n"), "{log}");
}
