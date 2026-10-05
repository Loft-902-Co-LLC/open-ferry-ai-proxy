//! Ports CLIProxyAPI internal/runtime/executor/helps/logging_helpers_test.go
//! (v8.0.15, MIT): what the taps record of the upstream attempts.
//!
//! Upstream's helpers (`RecordAPIRequest`, `AppendAPIResponseChunk`,
//! `RecordAPIResponseError`) are the request log's tap here, fed as an
//! executor feeds it.
//!
//! Dropped:
//! - `TestRequestLoggingDoesNotMarkUpstreamAttempt`, as the tap has no
//!   upstream attempt tracker to mark.
//! - `TestRecordAPIResponseMetadataStoresHeadersWhenRequestLogDisabled`, as
//!   the upstream response headers handlers read aren't kept by the request
//!   log.
//! - The file-backed cases of `TestAPIResponseAttemptsAreSeparated`, as
//!   bodies aren't spilled to files.

use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderMap, Method};

use super::{context, logger};
use crate::auth::Auth;
use crate::exec::{ErrorKind, ExecError, Format};
use crate::observe::{AttemptKind, AttemptRequest, Tap};

fn send(tap: &Arc<dyn Tap>, url: &str, body: &Bytes) {
    tap.attempt_request(&AttemptRequest {
        kind: AttemptKind::Execute,
        method: &Method::POST,
        url,
        headers: &HeaderMap::new(),
        body,
        provider: "codex",
        model: "gpt-5",
        format: &Format::from("codex"),
        auth: &Auth::default(),
        secrets: &crate::observe::redact::Secrets::new(),
    });
}

// Ports TestRecordAPIRequestClonesDeferredBodyWhenRequestLogDisabled.
#[test]
fn record_api_request_clones_deferred_body_when_request_log_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let logger = logger(dir.path(), false);
    let context = context("/v1/responses");
    logger.start(&context);
    let tap = logger.tap(&context).unwrap();
    let mut body = b"{\"model\":\"original\"}".to_vec();
    send(
        &tap,
        "https://api.example.com/v1/responses",
        &Bytes::copy_from_slice(&body),
    );
    body[10] = b'X';

    let mut captured = Vec::new();
    context
        .request_log()
        .with(|capture| captured = capture.attempts.deferred_requests());
    let captured = String::from_utf8(captured).unwrap();
    assert!(captured.contains("{\"model\":\"original\"}"), "{captured}");
    assert!(
        captured.starts_with("=== API REQUEST 1 ===\n"),
        "{captured}"
    );
}

// Ports TestAPIResponseAttemptsAreSeparated, its memory-backed cases.
#[test]
fn api_response_attempts_are_separated() {
    for first_body in [None, Some("partial")] {
        let dir = tempfile::tempdir().unwrap();
        let logger = logger(dir.path(), true);
        let context = context("/v1/responses");
        logger.start(&context);
        let tap = logger.tap(&context).unwrap();
        send(&tap, "https://api.example.com/first", &Bytes::new());
        match first_body {
            Some(body) => tap.chunk(&Bytes::from_static(body.as_bytes())),
            None => tap.error(&ExecError::new(ErrorKind::Upstream, "EOF")),
        }
        send(&tap, "https://api.example.com/second", &Bytes::new());
        tap.error(&ExecError::new(ErrorKind::Upstream, "retry failed"));
        drop(tap);

        let mut response = Vec::new();
        context
            .request_log()
            .with(|capture| response = capture.attempts.api_response());
        let response = String::from_utf8(response).unwrap();
        let previous_end = first_body.unwrap_or("Error: EOF");
        let boundary = format!("{previous_end}\n\n=== API RESPONSE 2 ===");
        assert!(
            response.contains(&boundary),
            "{response:?}\nwant {boundary:?}"
        );
    }
}
