//! Not upstream's: failed upstream attempts in the request log, through the
//! whole router. The executor's error goes to the attempt's `=== API
//! RESPONSE n ===` block, where upstream's executors record it
//! (`RecordAPIResponseError`), and the error the Responses handlers give
//! the client to an `=== API ERROR RESPONSE ===` section
//! (`LoggingAPIResponseError`), both with `request-log` on only, and
//! scrubbed of the credential's token. The Codex executor sends to a server
//! on 127.0.0.1 that fails.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use futures_util::{SinkExt, StreamExt};
use http::{HeaderValue, Request, StatusCode, header};
use open_ferry_core::auth::Auth;
use open_ferry_core::manager::{Manager, Settings};
use open_ferry_core::models::ModelInfo;
use open_ferry_core::registry::ModelRegistry;
use open_ferry_providers::codex::CodexExecutor;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::{Harness, send};
use crate::config::ServerConfig;
use crate::state::AppState;
use crate::testing::FakeCatalog;

/// The Codex sign-in's access token, which no log may show.
const TOKEN: &str = "codex-access-token-0123456789";

/// The client's key.
const CLIENT_KEY: &str = "test-key";

/// What the Codex executor answers a stream that ends before
/// `response.completed` with (upstream's `newCodexIncompleteStreamError`).
const INCOMPLETE: &str =
    "stream error: stream disconnected before completion: stream closed before response.completed";

/// A server on 127.0.0.1 that reads one request, answers it with `answer`
/// as it is, and closes the connection; its URL.
async fn failing_upstream(answer: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request(&mut stream).await;
        stream.write_all(answer.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
    });
    url
}

/// Reads a request's head and its body of `Content-Length` bytes.
async fn read_request(stream: &mut TcpStream) {
    let mut seen = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        if let Some(end) = seen.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&seen[..end]).to_ascii_lowercase();
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map_or(0, |length| length.trim().parse().unwrap());
            if seen.len() >= end + 4 + length {
                return;
            }
        }
        let read = stream.read(&mut buffer).await.unwrap();
        if read == 0 {
            return;
        }
        seen.extend_from_slice(&buffer[..read]);
    }
}

/// An answer with status 200, `Content-Type: text/event-stream`, a
/// `Content-Length` of `length` and `body`.
fn event_stream(length: usize, body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {length}\r\n\r\n{body}"
    )
}

/// An event stream that promises 1000 bytes and breaks off in its second
/// line.
fn cut_stream() -> String {
    event_stream(
        1000,
        "event: response.created\ndata: {\"type\":\"response.cre",
    )
}

/// A `response.failed` event whose message names the token.
fn failed_response() -> String {
    format!(
        "event: response.failed\ndata: {}\n\n",
        json!({
            "type": "response.failed",
            "response": {
                "id": "resp_1",
                "status": "failed",
                "error": {"code": "server_error", "message": format!("rejected Bearer {TOKEN}")},
            },
        })
    )
}

/// The whole router with `request-log` on or off, over a manager whose
/// Codex executor sends to `upstream`, with one Codex sign-in serving
/// `gpt-5`.
fn proxy(request_log: bool, upstream: &str) -> Harness {
    let registry = Arc::new(ModelRegistry::new());
    let manager = Arc::new(Manager::new(Settings::default(), registry.clone(), None));
    manager.register_executor(Arc::new(
        CodexExecutor::new("direct").with_base_url(format!("{upstream}/backend-api/codex")),
    ));
    let Value::Object(metadata) = json!({"access_token": TOKEN}) else {
        unreachable!()
    };
    manager
        .register_unsaved(Auth {
            id: "codex-auth".into(),
            provider: "codex".into(),
            metadata,
            ..Auth::default()
        })
        .unwrap();
    registry.register_client(
        "codex-auth",
        "codex",
        &[ModelInfo {
            id: "gpt-5".into(),
            ..ModelInfo::default()
        }],
    );
    Harness::over(
        request_log,
        AppState::new(
            client_keys(),
            manager,
            Arc::new(FakeCatalog::new().serve("gpt-5", &["codex"])),
        ),
    )
}

/// The server's config, with the client's key.
fn client_keys() -> ServerConfig {
    ServerConfig {
        api_keys: vec![CLIENT_KEY.into()],
        ..ServerConfig::default()
    }
}

/// A streamed Responses request for `gpt-5` with the client's key.
fn responses_stream() -> Request<Body> {
    Request::post("/v1/responses")
        .header(header::AUTHORIZATION, format!("Bearer {CLIENT_KEY}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            r#"{"model":"gpt-5","input":"hi","stream":true}"#,
        ))
        .unwrap()
}

/// The section of `log` that starts with `title`, up to the blank lines
/// before the next one.
fn section<'a>(log: &'a str, title: &str) -> &'a str {
    let start = log
        .find(title)
        .unwrap_or_else(|| panic!("no {title}: {log}"));
    let rest = &log[start..];
    let end = rest[title.len()..]
        .find("\n=== ")
        .map_or(rest.len(), |end| title.len() + end + 1);
    rest[..end].trim_end_matches('\n')
}

// Not upstream's: a stream that breaks off after its first event, before a
// whole frame reached the client. The read error follows the body read so
// far in the attempt's API RESPONSE block, then the incomplete stream's
// error, as upstream's Codex executor records both once the stream has
// started; the client's JSON error is in an API ERROR RESPONSE section.
#[tokio::test]
async fn logs_a_stream_cut_before_its_first_frame() {
    let upstream = failing_upstream(cut_stream()).await;
    let harness = proxy(true, &upstream);
    let app = crate::router(harness.state.clone());
    let (status, _, body) = send(&app, responses_stream()).await;
    assert_eq!(status, StatusCode::REQUEST_TIMEOUT, "{body:?}");
    let (name, log) = harness.only_log();
    assert!(name.starts_with("v1-responses-"), "{name}");

    assert_eq!(log.matches("=== API RESPONSE ").count(), 1, "{log}");
    let response = section(&log, "=== API RESPONSE 1 ===");
    assert!(response.contains("\nStatus: 200\n"), "{response}");
    assert!(
        response.contains(
            "Body:\nevent: response.created\ndata: {\"type\":\"response.creError: error decoding response body"
        ),
        "{response}"
    );
    assert!(
        response.ends_with(&format!("\n\nError: {INCOMPLETE}")),
        "{response}"
    );
    assert_eq!(response.matches("Error: ").count(), 2, "{response}");

    assert_eq!(
        section(&log, "=== API ERROR RESPONSE ==="),
        format!("=== API ERROR RESPONSE ===\nHTTP Status: 408\n{INCOMPLETE}")
    );
    assert!(!log.contains(TOKEN), "{log}");
}

// Not upstream's: a `response.failed` naming the credential's token. The
// executor's error follows the event in the attempt's API RESPONSE block,
// and the client's error is in an API ERROR RESPONSE section, the token
// scrubbed from both.
#[tokio::test]
async fn logs_a_failed_response_without_the_token() {
    let failed = failed_response();
    let upstream = failing_upstream(event_stream(failed.len(), &failed)).await;
    let harness = proxy(true, &upstream);
    let app = crate::router(harness.state.clone());
    let (status, _, body) = send(&app, responses_stream()).await;
    let error = r#"{"error":{"code":"server_error","message":"rejected Bearer [redacted]"}}"#;
    assert_eq!(
        (status, &body[..]),
        (StatusCode::BAD_GATEWAY, error.as_bytes())
    );
    let (_, log) = harness.only_log();
    assert!(!log.contains(TOKEN), "{log}");

    let response = section(&log, "=== API RESPONSE 1 ===");
    assert!(response.contains("\nStatus: 200\n"), "{response}");
    assert!(
        response.contains("Body:\nevent: response.failed\ndata: {"),
        "{response}"
    );
    assert!(
        response.ends_with(&format!("}}}}}}Error: {error}")),
        "{response}"
    );
    assert_eq!(
        section(&log, "=== API ERROR RESPONSE ==="),
        format!("=== API ERROR RESPONSE ===\nHTTP Status: 502\n{error}")
    );
}

// Not upstream's: with `request-log` off, the failed request's error log
// has the attempt's answer, its status, headers and errors, but not its
// body, which came with a 200; upstream's executors record no answer then.
// The handler's error has no API ERROR RESPONSE section, as upstream
// records it only with the request log on.
#[tokio::test]
async fn logs_the_attempts_answer_without_a_successful_body_with_the_request_log_off() {
    let upstream = failing_upstream(cut_stream()).await;
    let harness = proxy(false, &upstream);
    let app = crate::router(harness.state.clone());
    let (status, _, _) = send(&app, responses_stream()).await;
    assert_eq!(status, StatusCode::REQUEST_TIMEOUT);
    let (name, log) = harness.only_log();
    assert!(name.starts_with("error-v1-responses-"), "{name}");
    assert!(log.contains("=== API REQUEST 1 ===\n"), "{log}");
    assert!(log.contains("=== RESPONSE ===\nStatus: 408\n"), "{log}");

    let response = section(&log, "=== API RESPONSE 1 ===");
    assert!(response.contains("\nStatus: 200\nHeaders:\n"), "{response}");
    assert!(
        response.contains("\nContent-Type: text/event-stream\n"),
        "{response}"
    );
    assert!(
        response.contains("\n\nError: error decoding response body"),
        "{response}"
    );
    assert!(
        response.ends_with(&format!("\n\nError: {INCOMPLETE}")),
        "{response}"
    );
    assert_eq!(response.matches("Error: ").count(), 2, "{response}");
    assert!(!response.contains("Body:"), "{response}");
    for absent in [
        "response.created",
        "=== API RESPONSE 2",
        "=== API ERROR RESPONSE",
        TOKEN,
    ] {
        assert!(!log.contains(absent), "{absent}: {log}");
    }
}

// Not upstream's: a Responses WebSocket turn whose upstream stream fails.
// The session's log, written once it ends, has the attempt's API RESPONSE
// block with the executor's error, and the turn's error in an API ERROR
// RESPONSE section (upstream's `forwardResponsesWebsocket` records it),
// the token scrubbed from both.
#[tokio::test]
async fn logs_a_websocket_turns_error() {
    let failed = failed_response();
    let upstream = failing_upstream(event_stream(failed.len(), &failed)).await;
    let harness = proxy(true, &upstream);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/v1/responses", listener.local_addr().unwrap());
    let app = crate::router(harness.state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut request = url.into_client_request().unwrap();
    let bearer = HeaderValue::from_str(&format!("Bearer {CLIENT_KEY}")).unwrap();
    request.headers_mut().insert(header::AUTHORIZATION, bearer);
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    ws.send(Message::Text(
        r#"{"type":"response.create","model":"gpt-5","input":[]}"#.into(),
    ))
    .await
    .unwrap();
    while let Ok(Some(Ok(message))) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await
    {
        if message.is_close() {
            break;
        }
    }
    let _ = ws.close(None).await;
    drop(ws);

    let mut logs = Vec::new();
    for _ in 0..250 {
        logs = harness.logs();
        if !logs.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(logs.len(), 1, "{logs:#?}");
    let log = &logs[0].1;
    assert!(log.contains("Downstream Transport: websocket\n"), "{log}");
    assert!(!log.contains(TOKEN), "{log}");
    assert!(!log.contains(CLIENT_KEY), "{log}");
    let error = r#"{"error":{"code":"server_error","message":"rejected Bearer [redacted]"}}"#;
    let response = section(log, "=== API RESPONSE 1 ===");
    assert!(
        response.ends_with(&format!("}}}}}}Error: {error}")),
        "{response}"
    );
    assert_eq!(
        section(log, "=== API ERROR RESPONSE ==="),
        format!("=== API ERROR RESPONSE ===\nHTTP Status: 502\n{error}")
    );
}
