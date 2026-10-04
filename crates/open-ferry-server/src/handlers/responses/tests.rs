// Ported from CLIProxyAPI sdk/api/handlers/openai/openai_responses_handlers_stream_test.go,
// openai_responses_handlers_stream_error_test.go and
// openai_responses_compact_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Responses stream writer, from upstream's tests, and the routes end to
//! end against a fake dispatcher.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};
use http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{ErrorKind, ExecError, Format};
use open_ferry_core::observe::RequestContext;
use open_ferry_core::observe::request_log::{ApiError, RequestLogger};
use serde_json::Value;
use tower::ServiceExt;

use super::framer::Framer;
use super::stream_error::sanitize_error;
use super::{ResponsesWriter, is_codex_client, stream_error_diagnostic};
use crate::config::{ServerConfig, StreamingConfig};
use crate::errors::ErrorMessage;
use crate::router;
use crate::sse_check::MAX_EVENT_BYTES;
use crate::stream::{StreamWriter, forward};
use crate::testing::{FakeCatalog, FakeDispatcher, Outcome, state};

const CREATED: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-1\"}}\n\n";
const DELTA: &str = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n";
const COMPLETED: &str = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\",\"status\":\"completed\"}}\n\n";
const CYBER_POLICY: &str =
    r#"{"error":{"type":"invalid_request","code":"cyber_policy","message":"blocked"}}"#;
const CODEX_DESKTOP: &str = "Codex Desktop/26.803.41515";

fn ok(chunk: &str) -> Result<Bytes, ErrorMessage> {
    Ok(Bytes::copy_from_slice(chunk.as_bytes()))
}

fn failed(status: u16, text: &str) -> Result<Bytes, ErrorMessage> {
    Err(ErrorMessage::new(status, text))
}

/// What the stream writer sends for `items` once `framer` has started.
async fn run(framer: Framer, items: Vec<Result<Bytes, ErrorMessage>>) -> String {
    let body = forward(
        stream::iter(items).boxed(),
        ResponsesWriter::new(framer),
        None,
    );
    let bytes = body.collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// What the stream writer sends for `items` once `framer` has started,
/// recording its errors for the log of the request of `context`.
async fn run_logged(
    framer: Framer,
    items: Vec<Result<Bytes, ErrorMessage>>,
    context: &Arc<RequestContext>,
) -> String {
    let writer = ResponsesWriter::new(framer).log_to(Some(Arc::clone(context)));
    let body = forward(stream::iter(items).boxed(), writer, None);
    let bytes = body.collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// A request logged whole, as with upstream's `RequestLog: true`, whose
/// recorded errors the tests read as upstream's read `API_RESPONSE_ERROR`.
/// Its log is never written, as it never finishes.
fn logged_request() -> Arc<RequestContext> {
    let mut config = Config::default();
    config.request_log = true;
    let context = Arc::new(RequestContext::new(Method::POST, "/v1/responses".into()));
    let logger = RequestLogger::new(&config, Path::new("unwritten"), Path::new(""));
    assert!(logger.start(&context).is_some());
    context
}

/// The one error recorded for the log of the request of `context`.
fn only_api_error(context: &RequestContext) -> ApiError {
    let mut errors = context.request_log().api_errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    errors.remove(0)
}

/// A framer that has been given `chunk`, and what it wrote.
fn primed(codex: bool, chunk: &str) -> (Framer, String) {
    let mut framer = Framer::new(codex);
    let mut out = BytesMut::new();
    framer.write_chunk(&mut out, chunk.as_bytes()).unwrap();
    (framer, String::from_utf8(out.to_vec()).unwrap())
}

/// The `data:` payload of the last event in `body`.
fn last_payload(body: &str) -> Value {
    let event = body.trim().rsplit("\n\n").next().unwrap();
    let data = event
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap();
    serde_json::from_str(data).unwrap()
}

// TestForwardResponsesStreamSeparatesDataOnlySSEChunks
#[tokio::test]
async fn separates_data_only_chunks() {
    let body = run(
        Framer::new(false),
        vec![
            ok(
                r#"data: {"type":"response.output_item.done","item":{"type":"function_call","arguments":"{}"}}"#,
            ),
            ok(r#"data: {"type":"response.completed","response":{"id":"resp-1","output":[]}}"#),
        ],
    )
    .await;
    assert_eq!(
        body,
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"arguments\":\"{}\"}}\n\n\
         data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\",\"output\":[{\"type\":\"function_call\",\"arguments\":\"{}\"}]}}\n\n\n"
    );
}

// TestForwardResponsesStreamRepairsEmptyCompletedOutputFromDoneItems
#[tokio::test]
async fn repairs_empty_completed_output_from_done_items() {
    let reasoning = r#"{"type":"reasoning","id":"rs-1","summary":[]}"#;
    let call = r#"{"type":"function_call","id":"fc-1","call_id":"call-1","name":"shell","arguments":"{\"cmd\":\"pwd\"}","status":"completed"}"#;
    let body = run(
        Framer::new(false),
        vec![
            ok(&format!(
                r#"data: {{"type":"response.output_item.done","output_index":0,"item":{reasoning}}}"#
            )),
            ok(&format!(
                r#"data: {{"type":"response.output_item.done","output_index":1,"item":{call}}}"#
            )),
            ok(r#"data: {"type":"response.completed","response":{"id":"resp-1","output":[]}}"#),
        ],
    )
    .await;
    let parts: Vec<&str> = body.trim().split("\n\n").collect();
    assert_eq!(parts.len(), 3, "{body}");
    assert_eq!(
        parts[2],
        format!(
            r#"data: {{"type":"response.completed","response":{{"id":"resp-1","output":[{reasoning},{call}]}}}}"#
        )
    );
    let output = &last_payload(&body)["response"]["output"];
    assert_eq!(output[1]["name"], "shell");
    assert_eq!(output[1]["arguments"], r#"{"cmd":"pwd"}"#);
}

// TestForwardResponsesStreamRepairsMixedIndexedAndUnindexedDoneItems
#[tokio::test]
async fn repairs_mixed_indexed_and_unindexed_done_items() {
    let body = run(
        Framer::new(false),
        vec![
            ok(
                r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc-1","call_id":"call-1","name":"shell","arguments":"{}","status":"completed"}}"#,
            ),
            ok(
                r#"data: {"type":"response.output_item.done","item":{"type":"message","id":"msg-1","role":"assistant","content":[{"type":"output_text","text":"done"}]}}"#,
            ),
            ok(r#"data: {"type":"response.completed","response":{"id":"resp-1","output":[]}}"#),
        ],
    )
    .await;
    assert_eq!(body.trim().split("\n\n").count(), 3, "{body}");
    let output = last_payload(&body)["response"]["output"].clone();
    assert_eq!(output.as_array().unwrap().len(), 2);
    assert_eq!(output[0]["name"], "shell");
    assert_eq!(output[1]["id"], "msg-1");
}

// TestForwardResponsesStreamRepairsMultilineCompletedOutputAsSSEDataLines
#[tokio::test]
async fn repairs_multiline_completed_output_as_data_lines() {
    let body = run(
        Framer::new(false),
        vec![
            ok(
                r#"data: {"type":"response.output_item.done","item":{"type":"function_call","arguments":"{}"}}"#,
            ),
            ok(
                "data: {\"type\":\"response.completed\",\ndata: \"response\":{\"id\":\"resp-1\",\"output\":[]}}\n\n",
            ),
        ],
    )
    .await;
    let parts: Vec<&str> = body.trim().split("\n\n").collect();
    assert_eq!(parts.len(), 2, "{body}");
    assert_eq!(
        parts[1],
        "data: {\"type\":\"response.completed\",\n\
         data: \"response\":{\"id\":\"resp-1\",\"output\":[{\"type\":\"function_call\",\"arguments\":\"{}\"}]}}"
    );
}

// TestForwardResponsesStreamReassemblesSplitSSEEventChunks
#[tokio::test]
async fn reassembles_split_event_chunks() {
    let body = run(
        Framer::new(false),
        vec![
            ok("event: response.created"),
            ok(r#"data: {"type":"response.created","response":{"id":"resp-1"}}"#),
            ok("\n"),
        ],
    )
    .await;
    assert_eq!(
        body,
        format!(
            "{CREATED}\nevent: error\ndata: {{\"type\":\"error\",\"error\":{{\"code\":\"internal_server_error\",\
             \"message\":\"upstream stream closed before a terminal event (last event: response.created)\",\
             \"param\":null,\"type\":\"server_error\"}},\"sequence_number\":1}}\n\n"
        )
    );
}

// TestForwardResponsesStreamPreservesValidFullSSEEventChunks
#[tokio::test]
async fn preserves_valid_full_event_chunks() {
    let body = run(Framer::new(false), vec![ok(CREATED)]).await;
    assert!(body.starts_with(CREATED), "{body}");
    assert!(body.contains("event: error"), "{body}");
}

// TestForwardResponsesStreamBuffersSplitDataPayloadChunks
#[tokio::test]
async fn buffers_split_data_payload_chunks() {
    let body = run(
        Framer::new(false),
        vec![
            ok(r#"data: {"type":"response.created""#),
            ok(r#","response":{"id":"resp-1"}}"#),
        ],
    )
    .await;
    assert!(
        body.starts_with(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-1\"}}\n\n\nevent: error\n"
        ),
        "{body}"
    );
}

// TestForwardResponsesStreamDropsIncompleteTrailingDataChunkOnFlush
#[tokio::test]
async fn drops_incomplete_trailing_data_chunk_on_flush() {
    let body = run(
        Framer::new(false),
        vec![ok(r#"data: {"type":"response.created""#)],
    )
    .await;
    assert_eq!(
        body,
        "\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"code\":\"internal_server_error\",\
         \"message\":\"upstream stream closed before a terminal event (last event: none)\",\
         \"param\":null,\"type\":\"server_error\"},\"sequence_number\":0}\n\n"
    );
}

// TestForwardResponsesStreamErrorEventPreservesNestedError
#[tokio::test]
async fn error_event_preserves_nested_error() {
    let message = "This content was flagged for possible cybersecurity risk. If this seems wrong, \
                   try rephrasing your request. To get authorized for security work, join the \
                   Trusted Access for Cyber program: https://chatgpt.com/cyber";
    let error = format!(
        r#"{{"error":{{"type":"invalid_request","code":"cyber_policy","message":"{message}","param":null}}}}"#
    );
    let body = run(
        Framer::new(false),
        vec![
            ok("event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0}\n\n"),
            ok("event: response.in_progress\ndata: {\"type\":\"response.in_progress\",\"sequence_number\":1}\n\n"),
            failed(400, &error),
        ],
    )
    .await;
    let last = body.trim().rsplit("\n\n").next().unwrap();
    assert_eq!(
        last,
        format!(
            r#"event: error
data: {{"type":"error","error":{{"code":"cyber_policy","message":"{message}","param":null,"type":"invalid_request"}},"sequence_number":2}}"#
        )
    );
}

// TestForwardResponsesStreamErrorEventPreservesExplicitSequenceNumber
#[tokio::test]
async fn error_event_preserves_explicit_sequence_number() {
    let body = run(
        Framer::new(false),
        vec![failed(
            400,
            r#"{"error":{"type":"invalid_request","code":"custom"},"sequence_number":9}"#,
        )],
    )
    .await;
    assert_eq!(
        body,
        "\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"code\":\"custom\",\"type\":\"invalid_request\"},\"sequence_number\":9}\n\n"
    );
}

// TestForwardResponsesStream_FiltersUpstreamPrivateEvents
#[tokio::test]
async fn forward_filters_upstream_private_events() {
    let body = run(
        Framer::new(false),
        vec![
            ok("event: codex.rate_limits\ndata: {\"type\":\"codex.rate_limits\",\"rate_limits\":{\"primary\":{\"used_percent\":42}}}\n\n"),
            ok("event: codex.response.metadata\ndata: {\"type\":\"codex.response.metadata\",\"headers\":{\"x-turn-state\":\"turn-1\"}}\n\n"),
            ok(CREATED),
            ok("event: responsesapi.websocket_timing\ndata: {\"type\":\"responsesapi.websocket_timing\",\"timing\":{\"duration_ms\":100}}\n\n"),
            ok(COMPLETED),
        ],
    )
    .await;
    assert_eq!(body, format!("{CREATED}{COMPLETED}\n"));
}

// TestForwardResponsesStreamExposesTerminalErrors
#[tokio::test]
async fn exposes_terminal_errors() {
    let cases = [
        (400, CYBER_POLICY),
        (
            502,
            r#"{"error":{"type":"invalid_request","code":"cyber_policy","message":"This content was flagged for possible cybersecurity risk.","param":null}}"#,
        ),
        (
            502,
            r#"{"error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"Your input exceeds the context window."}}"#,
        ),
        (409, "conflict"),
        (413, "too large"),
        (422, "invalid input"),
        (401, "invalid credential"),
        (402, "insufficient credits"),
        (429, "usage limit reached"),
        (408, "upstream timeout"),
        (500, "unexpected EOF"),
        (
            500,
            r#"{"error":{"message":"websocket: close 1006 (abnormal closure): unexpected EOF","type":"server_error","code":"internal_server_error"}}"#,
        ),
    ];
    for (status, message) in cases {
        let body = run(Framer::new(false), vec![failed(status, message)]).await;
        assert!(body.contains(r#""type":"error""#), "{status}: {body}");
        assert!(body.contains("event: error\ndata: "), "{status}: {body}");
        assert!(body.contains(r#""error":{"#), "{status}: {body}");
    }
    let body = run(
        Framer::new(false),
        vec![failed(402, "insufficient credits")],
    )
    .await;
    assert_eq!(
        body,
        "\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"code\":\"invalid_request_error\",\
         \"message\":\"insufficient credits\",\"param\":null,\"type\":\"invalid_request_error\"},\
         \"sequence_number\":0}\n\n"
    );
}

// TestForwardResponsesStreamUsesResponseFailedForCodex
#[tokio::test]
async fn uses_response_failed_for_codex() {
    let body = run(Framer::new(true), vec![failed(400, CYBER_POLICY)]).await;
    assert_eq!(
        body,
        "\nevent: response.failed\ndata: {\"type\":\"response.failed\",\"sequence_number\":0,\
         \"response\":{\"status\":\"failed\",\"error\":{\"code\":\"cyber_policy\",\
         \"message\":\"blocked\",\"type\":\"invalid_request\"}}}\n\n"
    );
}

// TestForwardResponsesStreamExposesTransportErrorAfterOutputForCodex
#[tokio::test]
async fn exposes_transport_error_after_output_for_codex() {
    let (framer, written) = primed(true, DELTA);
    assert_eq!(written, DELTA);
    let error = sanitize_error(ErrorMessage::new(502, "unexpected EOF"));
    assert_eq!(
        stream_error_diagnostic(&framer, &error),
        (
            502,
            "responses stream terminated after response.output_text.delta: unexpected EOF"
                .to_owned()
        )
    );
    let context = logged_request();
    let body = run_logged(framer, vec![failed(502, "unexpected EOF")], &context).await;
    assert_eq!(
        body,
        "\nevent: response.failed\ndata: {\"type\":\"response.failed\",\"sequence_number\":1,\
         \"response\":{\"status\":\"failed\",\"error\":{\"code\":\"internal_server_error\",\
         \"message\":\"unexpected EOF\",\"param\":null,\"type\":\"server_error\"}}}\n\n"
    );
    assert_eq!(
        only_api_error(&context),
        ApiError {
            status: 502,
            message: "responses stream terminated after response.output_text.delta: unexpected EOF"
                .to_owned(),
            canceled: false,
        }
    );
}

// TestForwardResponsesStreamSanitizesDiagnosticErrorDetails
#[tokio::test]
async fn sanitizes_diagnostic_error_details() {
    let debug_secret = "super-secret-provider-debug-value";
    let message_secret = "super-secret-provider-message-value";
    let raw = format!(
        r#"{{"error":{{"type":"server_error","code":"upstream_failed","message":"upstream failed: {{\"api_key\":\"{message_secret}\"}}"}},"debug":{{"api_key":"{debug_secret}","trace":"{}"}}}}"#,
        "x".repeat(8192)
    );
    let (framer, _) = primed(false, DELTA);
    let (_, diagnostic) =
        stream_error_diagnostic(&framer, &sanitize_error(ErrorMessage::new(502, &raw)));
    assert!(!diagnostic.contains(debug_secret) && !diagnostic.contains(message_secret));
    assert!(diagnostic.len() <= 4096 && diagnostic.contains("upstream failed"));
    let context = logged_request();
    let body = run_logged(framer, vec![failed(502, &raw)], &context).await;
    assert_eq!(only_api_error(&context).message, diagnostic);
    assert!(
        body.contains("upstream failed") && body.contains("upstream_failed"),
        "{body}"
    );
    assert!(
        !body.contains(debug_secret) && !body.contains(message_secret),
        "{body}"
    );
    assert_eq!(
        last_payload(&body)["error"]["message"],
        r#"upstream failed: {"api_key":"[REDACTED]"}"#
    );
}

// TestForwardResponsesStreamPreservesNestedResponseError
#[tokio::test]
async fn preserves_nested_response_error() {
    let (framer, _) = primed(true, DELTA);
    let body = run(
        framer,
        vec![failed(
            502,
            r#"{"type":"response.failed","response":{"error":{"type":"server_error","code":"upstream_failed","message":"nested response failure","param":"input"}}}"#,
        )],
    )
    .await;
    assert_eq!(
        body,
        "\nevent: response.failed\ndata: {\"type\":\"response.failed\",\"sequence_number\":1,\
         \"response\":{\"status\":\"failed\",\"error\":{\"code\":\"upstream_failed\",\
         \"message\":\"nested response failure\",\"param\":\"input\",\"type\":\"server_error\"}}}\n\n"
    );
}

// TestForwardResponsesStreamSanitizesLastEventDiagnostic
#[tokio::test]
async fn sanitizes_last_event_diagnostic() {
    let event = format!("custom-event-Bearer event-secret-value{}", "x".repeat(1024));
    let (framer, _) = primed(
        false,
        &format!("event: {event}\ndata: {{\"message\":\"partial\"}}\n\n"),
    );
    let error = sanitize_error(ErrorMessage::new(502, "unexpected EOF"));
    let (_, diagnostic) = stream_error_diagnostic(&framer, &error);
    assert_eq!(
        diagnostic,
        "responses stream terminated after custom-event-Bearer [REDACTED]: unexpected EOF"
    );
    let context = logged_request();
    run_logged(framer, vec![failed(502, "unexpected EOF")], &context).await;
    assert_eq!(only_api_error(&context).message, diagnostic);
}

// TestForwardResponsesStreamSanitizesPayloadErrorsAndStopsAtFailure
#[tokio::test]
async fn sanitizes_payload_errors_and_stops_at_failure() {
    let frames = [
        "event: error\ndata: {\"type\":\"provider.error\",\"error\":{\"code\":\"failed\",\"message\":\"token=payload-secret\"}}\n\n",
        "data: {\"type\":\"provider.error\",\"error\":{\"code\":\"failed\",\"message\":\"token=payload-secret\"}}\n\n",
        "data: {\"code\":\"failed\",\"message\":\"token=payload-secret\"}\n\n",
    ];
    for frame in frames {
        let chunk = format!("{frame}{COMPLETED}");
        let mut writer = ResponsesWriter::new(Framer::new(true));
        writer.write_chunk(Bytes::from(chunk.clone()), &mut BytesMut::new());
        assert!(writer.chunk_error().is_some(), "{frame}");

        let body = run(Framer::new(true), vec![ok(&chunk)]).await;
        assert!(!body.contains("payload-secret"), "{body}");
        assert!(!body.contains("event: response.completed"), "{body}");
        assert_eq!(body.matches("event: response.failed").count(), 1, "{body}");
        assert!(body.contains("[REDACTED]"), "{body}");
    }
    let body = run(Framer::new(true), vec![ok(frames[2])]).await;
    assert_eq!(
        body,
        "event: response.failed\ndata: {\"type\":\"response.failed\",\"sequence_number\":0,\
         \"response\":{\"status\":\"failed\",\"error\":{\"code\":\"failed\",\
         \"message\":\"token=[REDACTED]\",\"param\":null,\"type\":\"server_error\"}}}\n\n"
    );
}

// TestForwardResponsesStreamReportsDataOnlyErrorFlushedAtEOF
#[tokio::test]
async fn reports_data_only_error_flushed_at_eof() {
    let chunk = r#"data: {"type":"error","error":{"message":"failed at EOF"}}"#;
    let context = logged_request();
    let mut writer = ResponsesWriter::new(Framer::new(true)).log_to(Some(Arc::clone(&context)));
    let mut out = BytesMut::new();
    writer.write_chunk(Bytes::from_static(chunk.as_bytes()), &mut out);
    assert!(writer.chunk_error().is_none());
    let error = writer.close_error().unwrap();
    assert!(error.text.contains("failed at EOF"), "{}", error.text);
    writer.write_terminal_error(&error, &mut out);
    let logged = only_api_error(&context).message;
    assert!(logged.contains("failed at EOF"), "{logged}");
    assert_eq!(
        &out[..],
        run(Framer::new(true), vec![ok(chunk)]).await.as_bytes()
    );
    assert_eq!(
        String::from_utf8(out.to_vec()).unwrap(),
        "event: response.failed\ndata: {\"type\":\"response.failed\",\"sequence_number\":0,\
         \"response\":{\"status\":\"failed\",\"error\":{\"message\":\"failed at EOF\"}}}\n\n"
    );
}

// TestForwardResponsesStreamDoesNotAppendFailureAfterTerminalEvent
#[tokio::test]
async fn does_not_append_failure_after_terminal_event() {
    let (framer, written) = primed(true, COMPLETED);
    assert_eq!(written, COMPLETED);
    let error = sanitize_error(ErrorMessage::new(502, "unexpected EOF after completion"));
    assert_eq!(
        stream_error_diagnostic(&framer, &error).1,
        "responses stream terminated after response.completed: unexpected EOF after completion"
    );
    let context = logged_request();
    let body = run_logged(
        framer,
        vec![failed(502, "unexpected EOF after completion")],
        &context,
    )
    .await;
    assert_eq!(body, "");
    assert_eq!(
        only_api_error(&context).message,
        "responses stream terminated after response.completed: unexpected EOF after completion"
    );
}

// TestForwardResponsesStreamFailsWhenUpstreamClosesWithoutTerminalEvent
#[tokio::test]
async fn fails_when_upstream_closes_without_terminal_event() {
    let (framer, _) = primed(true, DELTA);
    let body = run(framer, Vec::new()).await;
    assert_eq!(
        body,
        "\nevent: response.failed\ndata: {\"type\":\"response.failed\",\"sequence_number\":1,\
         \"response\":{\"status\":\"failed\",\"error\":{\"code\":\"internal_server_error\",\
         \"message\":\"upstream stream closed before a terminal event (last event: response.output_text.delta)\",\
         \"param\":null,\"type\":\"server_error\"}}}\n\n"
    );
}

#[test]
fn recognises_codex_clients() {
    let codex = |name: &str, value: &str| {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
        is_codex_client(&headers)
    };
    for agent in [
        "Codex Desktop/26.803.41515",
        " codex-tui/0.1",
        "codex_cli_rs",
        "codex_cli_rs/0.40.0 (Mac OS)",
        "codex_exec/1",
    ] {
        assert!(codex("user-agent", agent), "{agent}");
    }
    for agent in [
        "codex_vscode/0.153.4",
        "codex desktop/1",
        "Codex_cli_rs",
        "codex_cli_rsx",
    ] {
        assert!(!codex("user-agent", agent), "{agent}");
    }
    for originator in [
        "Codex Desktop",
        " CODEX-TUI ",
        "codex_cli_rs/x",
        "codex desktop/1",
    ] {
        assert!(codex("originator", originator), "{originator}");
    }
    for originator in ["codex_exec", "codex_vscode", "codex-tuix"] {
        assert!(!codex("originator", originator), "{originator}");
    }
    assert!(!is_codex_client(&HeaderMap::new()));
}

/// A server with `outcomes` that serves `gpt-5` through `codex`, with the
/// key `sk-test`.
fn app(config: ServerConfig, outcomes: Vec<Outcome>) -> (Router, Arc<FakeDispatcher>) {
    let catalog = FakeCatalog::new().serve("gpt-5", &["codex"]).first("gpt-5");
    let dispatcher = FakeDispatcher::new(outcomes);
    let config = ServerConfig {
        api_keys: vec!["sk-test".into()],
        ..config
    };
    (router(state(config, catalog, &dispatcher)), dispatcher)
}

/// A POST with the test key and `user_agent`, if any.
fn post(uri: &str, body: impl Into<Body>, user_agent: Option<&str>) -> Request<Body> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::AUTHORIZATION, "Bearer sk-test")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(user_agent) = user_agent {
        request = request.header(header::USER_AGENT, user_agent);
    }
    request.body(body.into()).unwrap()
}

/// The status, headers and body of `request`'s response.
async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (
        parts.status,
        parts.headers,
        String::from_utf8(bytes.to_vec()).unwrap(),
    )
}

/// The status and body of `request`'s response, and how many of the
/// dispatcher's streams were still held when the body's last bytes came.
/// Fails rather than waiting for ever on a body that doesn't end.
async fn send_to_end(
    app: &Router,
    dispatcher: &FakeDispatcher,
    request: Request<Body>,
) -> (StatusCode, String, usize) {
    tokio::time::timeout(Duration::from_secs(300), async {
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        let mut live = dispatcher.live_streams();
        while let Some(frame) = body.frame().await {
            bytes.extend_from_slice(&frame.unwrap().into_data().unwrap());
            live = dispatcher.live_streams();
        }
        (status, String::from_utf8(bytes).unwrap(), live)
    })
    .await
    .expect("the response never ended")
}

/// A stream of `first`, then 1 MiB chunks of `fill` until past
/// [`MAX_EVENT_BYTES`], that never ends.
fn overflowing(first: &[&str], fill: &[u8]) -> Outcome {
    let mut chunks: Vec<Result<Bytes, ExecError>> = first
        .iter()
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk.as_bytes())))
        .collect();
    let mut mib = fill.repeat((1 << 20) / fill.len());
    mib.resize(1 << 20, b'x');
    let mib = Bytes::from(mib);
    chunks.extend((0..=MAX_EVENT_BYTES >> 20).map(|_| Ok(mib.clone())));
    Outcome::Hang(HeaderMap::new(), chunks)
}

/// The 502 JSON error with `message`.
fn bad_gateway(message: &str) -> String {
    format!(
        r#"{{"error":{{"message":"{message}","type":"server_error","code":"internal_server_error"}}}}"#
    )
}

/// The `error` event ending a stream after one payload with `message`.
fn error_event(message: &str) -> String {
    format!(
        "\nevent: error\ndata: {{\"type\":\"error\",\"error\":{{\"code\":\"internal_server_error\",\
         \"message\":\"{message}\",\"param\":null,\"type\":\"server_error\"}},\"sequence_number\":1}}\n\n"
    )
}

fn content_type(headers: &HeaderMap) -> &str {
    headers
        .get(header::CONTENT_TYPE)
        .map_or("", |v| v.to_str().unwrap())
}

/// An error with no status, as an executor's plain Go error.
fn plain(message: &str) -> ExecError {
    ExecError::new(ErrorKind::Upstream, message)
}

const STREAM: &str = r#"{"model":"gpt-5","input":"hi","stream":true}"#;

#[tokio::test]
async fn answers_once_without_stream() {
    let (app, dispatcher) = app(
        ServerConfig::default(),
        vec![
            Outcome::reply(r#"{"id":"resp_1","object":"response"}"#),
            Outcome::reply("{}"),
        ],
    );
    let payload = r#"{"model":"gpt-5","input":"hi","stream":false}"#;
    let (status, headers, body) = send(&app, post("/v1/responses", payload, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "application/json");
    assert_eq!(body, r#"{"id":"resp_1","object":"response"}"#);

    let (status, _, _) = send(
        &app,
        post(
            "/v1/responses",
            r#"{"model":"gpt-5","stream":"true"}"#,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 2);
    let call = &calls[0];
    assert_eq!(call.method, "execute");
    assert_eq!(call.providers, ["codex"]);
    assert_eq!(call.request.model, "gpt-5");
    assert_eq!(&call.request.payload[..], payload.as_bytes());
    assert_eq!(call.options.source_format, Format::OPENAI_RESPONSE);
    assert_eq!(call.options.response_format, Format::OPENAI_RESPONSE);
    assert_eq!(call.options.alt, "");
    assert!(!call.options.stream);
    assert_eq!(calls[1].method, "execute");
}

#[tokio::test]
async fn streams_events() {
    let mut upstream = HeaderMap::new();
    upstream.insert("x-request-id", "req-1".parse().unwrap());
    let (app, dispatcher) = app(
        ServerConfig {
            passthrough_headers: true,
            ..ServerConfig::default()
        },
        vec![Outcome::Stream(
            upstream,
            vec![
                Ok(Bytes::from_static(CREATED.as_bytes())),
                Ok(Bytes::from_static(COMPLETED.as_bytes())),
            ],
        )],
    );
    let (status, headers, body) = send(&app, post("/v1/responses", STREAM, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "text/event-stream");
    assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
    assert_eq!(headers["x-request-id"], "req-1");
    assert_eq!(body, format!("{CREATED}{COMPLETED}\n"));

    let call = &dispatcher.calls()[0];
    assert_eq!(call.method, "execute_stream");
    assert_eq!(call.options.alt, "");
    assert!(call.options.stream);
    assert_eq!(&call.request.payload[..], STREAM.as_bytes());
}

#[tokio::test]
async fn ends_a_broken_stream_with_the_clients_failure_event() {
    let outcome = || {
        Outcome::Stream(
            HeaderMap::new(),
            vec![
                Ok(Bytes::from_static(DELTA.as_bytes())),
                Err(plain("unexpected EOF")),
            ],
        )
    };
    let (app, _) = app(ServerConfig::default(), vec![outcome(), outcome()]);

    // TestResponsesHandlerEmitsFailureWhenExecutorStopsAfterPartialOutput
    let (status, headers, body) =
        send(&app, post("/v1/responses", STREAM, Some(CODEX_DESKTOP))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "text/event-stream");
    assert_eq!(
        body,
        format!(
            "{DELTA}\nevent: response.failed\ndata: {{\"type\":\"response.failed\",\"sequence_number\":1,\
             \"response\":{{\"status\":\"failed\",\"error\":{{\"code\":\"internal_server_error\",\
             \"message\":\"unexpected EOF\",\"param\":null,\"type\":\"server_error\"}}}}}}\n\n"
        )
    );

    let (status, _, body) = send(
        &app,
        post("/v1/responses", STREAM, Some("codex_vscode/0.153.4")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        format!(
            "{DELTA}\nevent: error\ndata: {{\"type\":\"error\",\"error\":{{\"code\":\"internal_server_error\",\
             \"message\":\"unexpected EOF\",\"param\":null,\"type\":\"server_error\"}},\"sequence_number\":1}}\n\n"
        )
    );
}

// TestResponsesHandlerCommitsValidFrameBeforeMalformedFrameInSameChunk
#[tokio::test]
async fn commits_a_valid_frame_before_a_malformed_one() {
    let chunk = format!(
        "{DELTA}event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\"\n\n"
    );
    let (app, _) = app(
        ServerConfig::default(),
        vec![Outcome::chunks(&[chunk.as_str()])],
    );
    let (status, _, body) = send(&app, post("/v1/responses", STREAM, Some(CODEX_DESKTOP))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.starts_with(DELTA), "{body}");
    assert_eq!(body.matches("event: response.failed").count(), 1, "{body}");
    assert!(body.contains("invalid SSE data JSON"), "{body}");
}

// TestResponsesHandlerAcceptsMultilineDataAcrossExecutorChunks
#[tokio::test]
async fn accepts_multiline_data_across_chunks() {
    let (app, _) = app(
        ServerConfig::default(),
        vec![Outcome::chunks(&[
            "event: response.completed\ndata: {\"type\":\"response.completed\",",
            "data: \"response\":{\"id\":\"resp-1\",\"status\":\"completed\"}}\n\n",
        ])],
    );
    let (status, _, body) = send(&app, post("/v1/responses", STREAM, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        "event: response.completed\ndata: {\"type\":\"response.completed\",\n\
         data: \"response\":{\"id\":\"resp-1\",\"status\":\"completed\"}}\n\n\n"
    );
}

// TestResponsesHandlerFlushesDataOnlyFrameBeforeStreamingError
#[tokio::test]
async fn flushes_a_data_only_frame_before_an_error() {
    let data = r#"data: {"type":"response.output_text.delta","delta":"partial"}"#;
    let (app, _) = app(
        ServerConfig::default(),
        vec![Outcome::Stream(
            HeaderMap::new(),
            vec![
                Ok(Bytes::from_static(data.as_bytes())),
                Err(plain("upstream failed after data-only frame")),
            ],
        )],
    );
    let (status, _, body) = send(&app, post("/v1/responses", STREAM, Some(CODEX_DESKTOP))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        format!(
            "{data}\n\n\nevent: response.failed\ndata: {{\"type\":\"response.failed\",\"sequence_number\":1,\
             \"response\":{{\"status\":\"failed\",\"error\":{{\"code\":\"internal_server_error\",\
             \"message\":\"upstream failed after data-only frame\",\"param\":null,\"type\":\"server_error\"}}}}}}\n\n"
        )
    );
}

// TestResponsesHandlerEmitsFailureWhenDataOnlyStreamClosesCleanly
#[tokio::test]
async fn fails_a_data_only_stream_that_closes() {
    let data = r#"data: {"type":"response.output_text.delta","delta":"partial"}"#;
    let (app, _) = app(ServerConfig::default(), vec![Outcome::chunks(&[data])]);
    let (status, _, body) = send(&app, post("/v1/responses", STREAM, Some(CODEX_DESKTOP))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        format!(
            "{data}\n\n\nevent: response.failed\ndata: {{\"type\":\"response.failed\",\"sequence_number\":1,\
             \"response\":{{\"status\":\"failed\",\"error\":{{\"code\":\"internal_server_error\",\
             \"message\":\"upstream stream closed before a terminal event\",\"param\":null,\
             \"type\":\"server_error\"}}}}}}\n\n"
        )
    );
}

// TestResponsesHandlerDoesNotCommitHeadersForIncompleteFirstFrame
#[tokio::test]
async fn fails_with_json_before_the_first_whole_event() {
    let (app, _) = app(
        ServerConfig::default(),
        vec![Outcome::Stream(
            HeaderMap::new(),
            vec![
                Ok(Bytes::from_static(b"event: response.created")),
                Err(plain("upstream failed before first complete frame")),
            ],
        )],
    );
    let (status, headers, body) = send(&app, post("/v1/responses", STREAM, None)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(content_type(&headers), "application/json");
    assert_eq!(
        body,
        r#"{"error":{"message":"upstream failed before first complete frame","type":"server_error","code":"internal_server_error"}}"#
    );
}

// TestResponsesHandlerRejectsStreamClosedBeforeFirstPayload
#[tokio::test]
async fn rejects_a_stream_closed_before_its_first_payload() {
    let (app, _) = app(
        ServerConfig::default(),
        vec![Outcome::chunks(&[]), Outcome::chunks(&["event: ping\n\n"])],
    );
    for _ in 0..2 {
        let (status, _, body) = send(&app, post("/v1/responses", STREAM, None)).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(
            body,
            r#"{"error":{"message":"upstream stream closed before first payload","type":"server_error","code":"internal_server_error"}}"#
        );
    }
}

// TestResponsesHandlerDoesNotLoseErrorBeforeFirstPayload
#[tokio::test]
async fn keeps_an_error_before_the_first_payload() {
    let (app, _) = app(
        ServerConfig::default(),
        vec![
            Outcome::Stream(
                HeaderMap::new(),
                vec![Err(plain("upstream failed before first payload"))],
            ),
            Outcome::Fail(ExecError::upstream(429, "slow down")),
        ],
    );
    let (status, _, body) = send(&app, post("/v1/responses", STREAM, None)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        body.contains("upstream failed before first payload"),
        "{body}"
    );

    let (status, _, body) = send(&app, post("/v1/responses", STREAM, None)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        body,
        r#"{"error":{"message":"slow down","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#
    );
}

/// review3's error: serde_json can't read it, for its lone surrogate, so
/// its escaped `api_key` can't be redacted field by field.
const UNREADABLE_ERROR: &str =
    r#"{"error":{"message":"oops","api\u005fkey":"SECRET","note":"\ud800"}}"#;

#[tokio::test]
async fn fails_closed_on_an_error_it_cannot_redact() {
    let (app, _) = app(
        ServerConfig::default(),
        vec![Outcome::Fail(ExecError::upstream(502, UNREADABLE_ERROR))],
    );
    let request = post(
        "/v1/responses",
        r#"{"model":"gpt-5","stream":true,"input":[]}"#,
        None,
    );
    let (status, headers, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(content_type(&headers), "application/json");
    assert_eq!(
        body,
        r#"{"error":{"message":"Bad Gateway","type":"server_error","code":"internal_server_error"}}"#
    );
}

#[tokio::test]
async fn fails_closed_on_an_error_it_cannot_redact_mid_stream() {
    let (framer, _) = primed(false, CREATED);
    let body = run(framer, vec![failed(502, UNREADABLE_ERROR)]).await;
    assert!(!body.contains("SECRET"), "{body}");
    assert_eq!(last_payload(&body)["error"]["message"], "Bad Gateway");

    let event = concat!(
        "event: error\n",
        r#"data: {"type":"error","status":503,"error":{"password":"SECRET","note":"\ud800"}}"#,
        "\n\n"
    );
    let (framer, written) = primed(true, event);
    assert!(!written.contains("SECRET"), "{written}");
    assert_eq!(
        last_payload(&written)["response"]["error"]["message"],
        "Service Unavailable"
    );
    assert_eq!(
        framer.terminal_error.map(|error| error.text).as_deref(),
        Some("Service Unavailable")
    );
}

// TestResponsesHandlerSanitizesErrorBeforeFirstFrame
#[tokio::test]
async fn sanitizes_an_error_before_the_first_event() {
    let error = format!(
        r#"{{"error":{{"type":"server_error","code":"upstream_failed","message":"initial upstream failure: {{\"api_key\":\"initial-message-secret\"}}"}},"debug":{{"token":"initial-debug-secret","trace":"{}"}}}}"#,
        "x".repeat(8192)
    );
    let (app, _) = app(
        ServerConfig::default(),
        vec![Outcome::Stream(HeaderMap::new(), vec![Err(plain(&error))])],
    );
    let (status, _, body) = send(&app, post("/v1/responses", STREAM, None)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body,
        r#"{"error":{"code":"upstream_failed","message":"initial upstream failure: {\"api_key\":\"[REDACTED]\"}","type":"server_error"}}"#
    );
}

#[tokio::test]
async fn ends_without_a_failure_after_a_failed_event() {
    let failure = "event: response.failed\ndata: {\"type\":\"response.failed\",\"sequence_number\":3,\"response\":{\"error\":{\"code\":\"server_error\",\"message\":\"boom\"}}}\n\n";
    let (app, _) = app(
        ServerConfig::default(),
        vec![Outcome::chunks(&[failure, COMPLETED])],
    );
    let (status, _, body) = send(&app, post("/v1/responses", STREAM, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        "event: error\ndata: {\"type\":\"error\",\"error\":{\"code\":\"server_error\",\"message\":\"boom\"},\
         \"sequence_number\":3}\n\n"
    );
}

#[tokio::test(start_paused = true)]
async fn streams_keep_alive_while_the_provider_is_quiet() {
    let config = ServerConfig {
        streaming: StreamingConfig {
            keepalive: Some(Duration::from_secs(5)),
            ..StreamingConfig::default()
        },
        ..ServerConfig::default()
    };
    let (app, _) = app(
        config,
        vec![Outcome::Hang(
            HeaderMap::new(),
            vec![Ok(Bytes::from_static(CREATED.as_bytes()))],
        )],
    );
    let response = app
        .oneshot(post("/v1/responses", STREAM, None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let mut next = async || {
        let frame = body.frame().await.unwrap().unwrap();
        String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap()
    };
    assert_eq!(next().await, CREATED);
    assert_eq!(next().await, ": keep-alive\n\n");
    assert_eq!(next().await, ": keep-alive\n\n");
}

// An event the provider's stream is checked for that grows past the limit
// fails the stream, and the call is dropped while the provider is still
// sending: with a 502 before the first payload, and with the stream's
// failure event after it.
#[tokio::test]
async fn fails_a_checked_event_past_the_limit() {
    let too_large = format!("upstream SSE event exceeds {MAX_EVENT_BYTES} bytes");
    let data = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"";
    let (app, dispatcher) = app(
        ServerConfig::default(),
        vec![
            overflowing(&[data], b"x"),
            overflowing(&[CREATED, data], b"x"),
        ],
    );
    let request = || post("/v1/responses", STREAM, None);
    let (status, body, live) = send_to_end(&app, &dispatcher, request()).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body, bad_gateway(&too_large));
    assert_eq!(live, 0);

    let (status, body, live) = send_to_end(&app, &dispatcher, request()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, format!("{CREATED}{}", error_event(&too_large)));
    assert_eq!(live, 0);
}

// Good events in the chunk that takes an event past the limit go first, and
// the stream fails at once, without waiting for the provider's next chunk.
#[tokio::test]
async fn fails_at_once_after_good_events_in_the_same_chunk() {
    let mut chunk = format!("{CREATED}data: {{\"delta\":\"").into_bytes();
    chunk.resize(chunk.len() + MAX_EVENT_BYTES + 1, b'x');
    let (app, dispatcher) = app(
        ServerConfig::default(),
        vec![Outcome::Hang(
            HeaderMap::new(),
            vec![Ok(Bytes::from(chunk))],
        )],
    );
    let request = post("/v1/responses", STREAM, None);
    let (status, body, live) = send_to_end(&app, &dispatcher, request).await;
    assert_eq!(status, StatusCode::OK);
    let too_large = format!("upstream SSE event exceeds {MAX_EVENT_BYTES} bytes");
    assert_eq!(body, format!("{CREATED}{}", error_event(&too_large)));
    assert_eq!(live, 0);
}

// The same for an event the framer holds back: data that is whole, waiting
// for its name, followed by text that is no event.
#[tokio::test]
async fn fails_a_framed_event_past_the_limit() {
    let too_large = format!("upstream SSE event exceeds {MAX_EVENT_BYTES} bytes");
    let (app, dispatcher) = app(
        ServerConfig::default(),
        vec![
            overflowing(&["data: {}"], b"x"),
            overflowing(&[CREATED, "data: {}"], b"x"),
        ],
    );
    let request = || post("/v1/responses", STREAM, None);
    let (status, body, live) = send_to_end(&app, &dispatcher, request()).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body, bad_gateway(&too_large));
    assert_eq!(live, 0);

    let (status, body, live) = send_to_end(&app, &dispatcher, request()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, format!("{CREATED}{}", error_event(&too_large)));
    assert_eq!(live, 0);
}

// What comes before the first payload is held back up to the limit too.
#[tokio::test]
async fn fails_a_stream_with_too_much_before_its_first_payload() {
    let mut comment = b": ".to_vec();
    comment.resize((1 << 20) - 2, b'x');
    comment.extend_from_slice(b"\n\n");
    let (app, dispatcher) = app(ServerConfig::default(), vec![overflowing(&[], &comment)]);
    let request = post("/v1/responses", STREAM, None);
    let (status, body, live) = send_to_end(&app, &dispatcher, request).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        body,
        bad_gateway(&format!(
            "upstream stream sent more than {MAX_EVENT_BYTES} bytes before its first payload"
        ))
    );
    assert_eq!(live, 0);
}

// A large event under the limit, in small chunks, goes through whole.
#[tokio::test]
async fn passes_a_large_event() {
    let mut event = b"event: response.output_text.delta\n\
        data: {\"type\":\"response.output_text.delta\",\"delta\":\""
        .to_vec();
    event.resize((8 << 20) - 4, b'x');
    event.extend_from_slice(b"\"}\n\n");
    let event = Bytes::from(event);
    let mut chunks = vec![Ok(Bytes::from_static(CREATED.as_bytes()))];
    chunks.extend(
        (0..event.len())
            .step_by(1024)
            .map(|at| Ok(event.slice(at..at + 1024))),
    );
    chunks.push(Ok(Bytes::from_static(COMPLETED.as_bytes())));
    let (app, dispatcher) = app(
        ServerConfig::default(),
        vec![Outcome::Stream(HeaderMap::new(), chunks)],
    );
    let request = post("/v1/responses", STREAM, None);
    let (status, body, _) = send_to_end(&app, &dispatcher, request).await;
    assert_eq!(status, StatusCode::OK);
    let event = std::str::from_utf8(&event).unwrap();
    assert!(
        body == format!("{CREATED}{event}{COMPLETED}\n"),
        "{}",
        body.len()
    );
}

// TestOpenAIResponsesCompactRejectsStream
#[tokio::test]
async fn compact_rejects_a_stream() {
    let (app, dispatcher) = app(ServerConfig::default(), Vec::new());
    let (status, headers, body) = send(
        &app,
        post(
            "/v1/responses/compact",
            r#"{"model":"gpt-5","stream":true}"#,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(content_type(&headers), "application/json; charset=utf-8");
    assert_eq!(
        body,
        r#"{"error":{"message":"Streaming not supported for compact responses","type":"invalid_request_error"}}"#
    );
    assert!(dispatcher.calls().is_empty());
}

// TestOpenAIResponsesCompactExecute
#[tokio::test]
async fn compact_calls_the_compact_variant() {
    let (app, dispatcher) = app(
        ServerConfig::default(),
        vec![Outcome::reply(r#"{"ok":true}"#), Outcome::reply("{}")],
    );
    let payload = r#"{"model":"gpt-5","input":"hello"}"#;
    let (status, headers, body) = send(&app, post("/v1/responses/compact", payload, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "application/json");
    assert_eq!(body, r#"{"ok":true}"#);

    let with_stream = "{ \"model\":\"gpt-5\", \"stream\" : false , \"input\":\"hi\"}";
    let (status, _, _) = send(&app, post("/v1/responses/compact", with_stream, None)).await;
    assert_eq!(status, StatusCode::OK);

    let calls = dispatcher.calls();
    assert_eq!(calls[0].method, "execute");
    assert_eq!(calls[0].options.alt, "responses/compact");
    assert_eq!(calls[0].options.source_format, Format::OPENAI_RESPONSE);
    assert!(!calls[0].options.stream);
    assert_eq!(&calls[0].request.payload[..], payload.as_bytes());
    // sjson cuts from the comma before the key to the end of the value.
    assert_eq!(
        &calls[1].request.payload[..],
        b"{ \"model\":\"gpt-5\" , \"input\":\"hi\"}"
    );
}

// TestOpenAIResponsesCompactDecodesZstdRequestBody
#[tokio::test]
async fn compact_decodes_a_zstd_body() {
    let (app, dispatcher) = app(
        ServerConfig::default(),
        vec![Outcome::reply(r#"{"ok":true}"#)],
    );
    let payload = br#"{"model":"gpt-5","input":"hello","stream":null}"#;
    let compressed = ruzstd::encoding::compress_to_vec(
        &payload[..],
        ruzstd::encoding::CompressionLevel::Fastest,
    );
    let mut request = post("/v1/responses/compact", compressed, None);
    request
        .headers_mut()
        .insert(header::CONTENT_ENCODING, "zstd".parse().unwrap());
    let (status, _, body) = send(&app, request).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, r#"{"ok":true}"#));
    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].options.alt, "responses/compact");
    assert_eq!(
        &calls[0].request.payload[..],
        br#"{"model":"gpt-5","input":"hello"}"#
    );
}

// TestOpenAIResponsesCompactTransientFailureDoesNotCooldownAuthAndPreservesError
// and TestOpenAIResponsesCompactRequestFaultStopsFallbackAndPreservesError
#[tokio::test]
async fn compact_passes_errors_on() {
    let error = r#"{"error":{"message":"compact upstream temporary error","type":"api_error"}}"#;
    let (app, dispatcher) = app(
        ServerConfig::default(),
        vec![
            Outcome::Fail(ExecError::upstream(500, error)),
            Outcome::Fail(ExecError::upstream(404, "404 page not found")),
        ],
    );
    let payload = r#"{"model":"gpt-5","input":"hello"}"#;
    let (status, headers, body) = send(&app, post("/v1/responses/compact", payload, None)).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::INTERNAL_SERVER_ERROR, error)
    );
    assert_eq!(content_type(&headers), "application/json");

    let (status, _, body) = send(&app, post("/v1/responses/compact", payload, None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        r#"{"error":{"message":"404 page not found","type":"invalid_request_error","code":"model_not_found"}}"#
    );
    assert_eq!(dispatcher.calls().len(), 2);
}

#[tokio::test]
async fn serves_the_codex_backend_paths() {
    let (app, dispatcher) = app(
        ServerConfig::default(),
        vec![
            Outcome::reply(r#"{"id":"resp_1"}"#),
            Outcome::reply(r#"{"ok":true}"#),
            Outcome::chunks(&[COMPLETED]),
        ],
    );
    let payload = r#"{"model":"gpt-5","input":"hi"}"#;
    let (status, _, body) = send(&app, post("/backend-api/codex/responses", payload, None)).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, r#"{"id":"resp_1"}"#)
    );
    let (status, _, body) = send(
        &app,
        post("/backend-api/codex/responses/compact", payload, None),
    )
    .await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, r#"{"ok":true}"#));
    let (status, _, body) = send(&app, post("/backend-api/codex/responses", STREAM, None)).await;
    assert_eq!((status, body), (StatusCode::OK, format!("{COMPLETED}\n")));

    let calls = dispatcher.calls();
    assert_eq!(calls[0].options.alt, "");
    assert_eq!(calls[1].options.alt, "responses/compact");
    assert_eq!(calls[2].method, "execute_stream");
}

// TestOpenAIResponsesForwardsInvalidReasoningEncryptedContentToExecutor and
// TestOpenAIResponsesCompactForwardsInvalidReasoningEncryptedContentToExecutor
#[tokio::test]
async fn forwards_reasoning_content_unchecked() {
    let (app, dispatcher) = app(
        ServerConfig::default(),
        vec![Outcome::reply("{}"), Outcome::reply("{}")],
    );
    let payload = concat!(
        r#"{"model":"gpt-5","stream":false,"input":[{"id":"rs_bad","type":"reasoning","encrypted_content":"gAAAAABqFTIa\"#,
        r#"u2026abc","summary":[]}]}"#
    );
    let (status, _, _) = send(&app, post("/v1/responses", payload, None)).await;
    assert_eq!(status, StatusCode::OK);
    let compact = r#"{"model":"gpt-5","input":[{"id":"rs_bad","type":"reasoning","encrypted_content":"bad","summary":[]}]}"#;
    let (status, _, _) = send(&app, post("/v1/responses/compact", compact, None)).await;
    assert_eq!(status, StatusCode::OK);

    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(&calls[0].request.payload[..], payload.as_bytes());
    assert_eq!(&calls[1].request.payload[..], compact.as_bytes());
    assert_eq!(calls[1].options.alt, "responses/compact");
}
