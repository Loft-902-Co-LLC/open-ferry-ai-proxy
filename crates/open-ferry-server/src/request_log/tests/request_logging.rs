//! Ports CLIProxyAPI internal/api/middleware/request_logging_test.go
//! (v8.0.10, MIT).
//!
//! Upstream's executor helpers (`helps.RecordAPIRequest`,
//! `helps.AppendAPIResponseChunk`) are the request's taps here, fed as an
//! executor feeds them.
//!
//! Changed:
//! - `TestShouldSkipMethodForRequestLogging` has no nil request.
//! - `TestShouldCaptureRequestBody` checks how a body is kept: read ahead
//!   with `request-log` on; else kept as the handler reads it, unless it is
//!   empty or a multipart form, whatever its size.
//! - `TestDeferredRequestBodyCaptureDoesNotDrainUnreadBody` reads a frame
//!   rather than a byte.
//! - `TestRequestLoggingMiddleware_ClientCancellationExclusion`'s canceled
//!   context is a client that drops the answer before it ends.
//! - `TestRequestLoggingMiddleware_PreservesFullUUIDForLoggerAndTruncatesFilename`
//!   and `TestRequestLoggingMiddleware_StreamingPreservesFullUUIDForLogger`
//!   check the file name, as the logger is not a spy.
//! - `TestDecodeCapturedRequestBodyForLogWithLimitTruncatesZstdExpansion` is
//!   in open-ferry-core's `request_log/format.rs`, with the decoding.
//!
//! Dropped:
//! - `TestAttachRequestLogSourcesUsesLoggerLogsDir`, as bodies aren't
//!   spilled to files.
//! - `TestCaptureRequestInfo_HeadersDeepCopy`, as the headers kept are a
//!   copy of their own.
//!
//! Added: a body past the server's limit, a body the handler doesn't read,
//! a client that leaves before the answer or during it, and a WebSocket
//! upgrade.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use futures_util::{StreamExt as _, stream};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use http_body_util::BodyExt as _;
use open_ferry_core::observe::request_log::{DeferredCapture, Mode};
use open_ferry_core::observe::{AttemptKind, RequestContext};
use tower::ServiceExt as _;

use super::super::{
    Capture, TeeBody, capture_plan, is_responses_websocket_upgrade, should_log_path, skips_method,
};
use super::{Harness, Upstream, post as post_request, send};
use crate::config::ServerConfig;
use crate::state::AppState;

fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.append(*name, HeaderValue::from_static(value));
    }
    headers
}

// Ports TestShouldSkipMethodForRequestLogging.
#[test]
fn should_skip_method_for_request_logging() {
    let upgrade = headers(&[("upgrade", "websocket")]);
    let none = HeaderMap::new();
    assert!(!skips_method(&Method::POST, "/v1/responses", &none));
    assert!(skips_method(&Method::GET, "/v1/models", &none));
    assert!(!skips_method(&Method::GET, "/v1/responses", &upgrade));
    assert!(!skips_method(
        &Method::GET,
        "/backend-api/codex/responses",
        &upgrade
    ));
    assert!(skips_method(&Method::GET, "/v1/responses", &none));
    assert!(is_responses_websocket_upgrade(
        "/v1/responses",
        &headers(&[("upgrade", " WebSocket ")])
    ));
}

// Ports TestShouldCaptureRequestBody.
#[test]
fn should_capture_request_body() {
    let json = headers(&[("content-type", "application/json")]);
    let multipart = headers(&[("content-type", "multipart/form-data; boundary=abc")]);
    assert_eq!(capture_plan(Mode::Full, &json, None), Capture::Eager);
    assert_eq!(
        capture_plan(Mode::ErrorsOnly, &json, Some(2)),
        Capture::Deferred
    );
    assert_eq!(
        capture_plan(Mode::ErrorsOnly, &json, Some((1 << 20) + 1)),
        Capture::Deferred
    );
    assert_eq!(
        capture_plan(Mode::ErrorsOnly, &json, None),
        Capture::Deferred
    );
    assert_eq!(
        capture_plan(Mode::ErrorsOnly, &multipart, Some(1)),
        Capture::None
    );
    assert_eq!(
        capture_plan(Mode::ErrorsOnly, &json, Some(0)),
        Capture::None
    );
}

// Ports TestDeferredRequestBodyCaptureDoesNotDrainUnreadBody.
#[tokio::test]
async fn deferred_request_body_capture_does_not_drain_unread_body() {
    let capture = DeferredCapture::new(None);
    let chunks = stream::iter(["r", "emaining-body"].map(Ok::<_, std::io::Error>));
    let mut body = TeeBody {
        inner: Body::from_stream(chunks),
        capture: capture.clone(),
    };
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(first, "r");
    let debug = format!("{capture:?}");
    assert!(debug.contains("captured: 1"), "{debug}");
    assert!(debug.contains("saw_eof: false"), "{debug}");
    let rest = body.collect().await.unwrap().to_bytes();
    assert_eq!(rest, "emaining-body");
}

/// What a handler sends back: the request's ID in a header.
fn with_id(context: &RequestContext, response: impl IntoResponse) -> Response {
    let mut response = response.into_response();
    response
        .headers_mut()
        .insert("x-test-request-id", context.id.as_str().parse().unwrap());
    response
}

// Ports TestRequestLoggingMiddlewareCapturesLargeErrorRequestAndDeferredAPIRequest.
#[tokio::test]
async fn request_logging_middleware_captures_large_error_request_and_deferred_api_request() {
    let harness = Harness::new(false);
    let mut payload = b"{\"marker\":\"large-error-body\",\"padding\":\"".to_vec();
    payload.extend(std::iter::repeat_n(b'x', 1 << 20));
    payload.extend_from_slice(b"\"}");
    let expected = Bytes::from(payload);
    let upstream_body = "{\"model\":\"upstream-model\",\"input\":\"translated\"}";

    let want = expected.clone();
    let app = harness.app(Router::new().route(
        "/v1/responses",
        post(
            move |State(state): State<AppState>,
                  Extension(context): Extension<Arc<RequestContext>>,
                  body: Bytes| async move {
                if body != want {
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
                Upstream {
                    body: upstream_body,
                    ..Upstream::default()
                }
                .run(&state, &context);
                (
                    StatusCode::BAD_REQUEST,
                    [("content-type", "application/json")],
                    "{\"error\":\"upstream rejected request\"}",
                )
                    .into_response()
            },
        ),
    ));

    let (status, _, _) = send(&app, post_request("/v1/responses", expected.clone())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (name, log) = harness.only_log();
    assert!(
        name.starts_with("error-") && name.ends_with(".log"),
        "{name}"
    );
    assert!(log.contains(&*String::from_utf8_lossy(&expected)));
    assert!(log.contains("=== API REQUEST 1 ==="));
    assert!(log.contains(upstream_body));
}

// Ports TestRequestLoggingMiddleware_StreamingResponsesUpstreamSections.
#[tokio::test]
async fn request_logging_middleware_streaming_responses_upstream_sections() {
    let harness = Harness::new(true);
    let event = "data: {\"type\":\"response.output_item.added\"}\n\n";
    let app = harness.app(Router::new().route(
        "/v1/responses",
        post(
            move |State(state): State<AppState>,
                  Extension(context): Extension<Arc<RequestContext>>| async move {
                Upstream {
                    kind: AttemptKind::Stream,
                    body: "{\"model\":\"gpt-5-codex\",\"input\":[]}",
                    chunks: &[event],
                    ..Upstream::default()
                }
                .run(&state, &context);
                ([("content-type", "text/event-stream")], event)
            },
        ),
    ));

    let (status, _, _) = send(
        &app,
        post_request(
            "/v1/responses",
            "{\"model\":\"gpt-5-codex\",\"input\":[],\"stream\":true}",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (name, log) = harness.only_log();
    assert!(
        name.starts_with("v1-responses-") && name.ends_with(".log"),
        "{name}"
    );
    assert!(log.contains("=== API REQUEST 1 ==="), "{log}");
    assert!(log.contains("=== API RESPONSE 1 ==="), "{log}");
    assert!(log.ends_with(&format!("\n\n{event}")), "{log:?}");
}

// Ports TestCaptureRequestInfoDecodesZstdRequestBodyForLog.
#[tokio::test]
async fn capture_request_info_decodes_zstd_request_body_for_log() {
    let harness = Harness::new(true);
    let payload = "{\"model\":\"test-model\",\"stream\":true}";
    let compressed = Bytes::from(ruzstd::encoding::compress_to_vec(
        payload.as_bytes(),
        ruzstd::encoding::CompressionLevel::Fastest,
    ));
    let want = compressed.clone();
    let app = harness.app(Router::new().route(
        "/v1/responses",
        post(move |body: Bytes| async move {
            if body == want {
                StatusCode::OK
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }),
    ));
    let request = Request::post("/v1/responses")
        .header("content-encoding", "zstd")
        .body(Body::from(compressed))
        .unwrap();
    let (status, _, _) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK, "the handler got other bytes");
    let (_, log) = harness.only_log();
    assert!(
        log.contains(&format!("=== REQUEST BODY ===\n{payload}\n\n")),
        "{log}"
    );
}

/// A route answering `/v1/responses` with `status`.
fn answering(status: u16) -> Router<AppState> {
    Router::new().route(
        "/v1/responses",
        post(move || async move { StatusCode::from_u16(status).unwrap() }),
    )
}

// Ports TestRequestLoggingMiddleware_ClientCancellationExclusion.
#[tokio::test]
async fn request_logging_middleware_client_cancellation_exclusion() {
    // 499 status does not create error log when request-log is false.
    let harness = Harness::new(false);
    let app = harness.app(answering(499));
    let (status, _, _) = send(&app, post_request("/v1/responses", "{\"model\":\"gpt-4\"}")).await;
    assert_eq!(status.as_u16(), 499);
    assert!(harness.logs().is_empty());

    // A client that leaves does not create error log when request-log is
    // false.
    let harness = Harness::new(false);
    let app = harness.app(Router::new().route(
        "/v1/responses",
        post(|| async { Body::from_stream(stream::pending::<Result<Bytes, std::io::Error>>()) }),
    ));
    let response = app
        .clone()
        .oneshot(post_request("/v1/responses", "{\"model\":\"gpt-4\"}"))
        .await
        .unwrap();
    drop(response);
    assert!(harness.logs().is_empty());

    // 400 bad request creates error log when request-log is false.
    let harness = Harness::new(false);
    let app = harness.app(Router::new().route(
        "/v1/responses",
        post(|| async {
            (
                StatusCode::BAD_REQUEST,
                [("content-type", "application/json")],
                "{\"error\":\"invalid parameter\"}",
            )
        }),
    ));
    let (status, _, _) = send(&app, post_request("/v1/responses", "{\"bad\":\"param\"}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let logs = harness.logs();
    let errors = logs
        .iter()
        .filter(|(name, _)| name.starts_with("error-") && name.ends_with(".log"))
        .count();
    assert_eq!(errors, 1, "{logs:?}");

    // 499 status logs standard request when request-log is true.
    let harness = Harness::new(true);
    let app = harness.app(answering(499));
    send(&app, post_request("/v1/responses", "{\"model\":\"gpt-4\"}")).await;
    let logs = harness.logs();
    let standard = logs
        .iter()
        .filter(|(name, _)| !name.starts_with("error-") && name.ends_with(".log"))
        .count();
    assert_eq!(standard, 1, "{logs:?}");
}

// Ports TestManagementV8RequestsAreNotLogged.
#[test]
fn management_v8_requests_are_not_logged() {
    for path in [
        "/v8/management/config",
        "/v8/management/config.yaml",
        "/v8/management/config/api-keys/codex",
        "/v8/management/oauth/auth-url",
    ] {
        assert!(!should_log_path(path), "{path}");
    }
    assert!(should_log_path("/v1/chat/completions"));
}

// Ports TestRequestLoggingMiddleware_PreservesFullUUIDForLoggerAndTruncatesFilename
// and TestRequestLoggingMiddleware_StreamingPreservesFullUUIDForLogger.
#[tokio::test]
async fn request_logging_middleware_preserves_full_uuid_and_truncates_filename() {
    for streaming in [false, true] {
        let harness = Harness::new(true);
        let app = harness.app(Router::new().route(
            "/v1/chat/completions",
            post(
                move |Extension(context): Extension<Arc<RequestContext>>| async move {
                    if streaming {
                        with_id(
                            &context,
                            ([("content-type", "text/event-stream")], "data: chunk\n\n"),
                        )
                    } else {
                        with_id(
                            &context,
                            (
                                [("content-type", "application/json")],
                                "{\"choices\":[\"hello\"]}",
                            ),
                        )
                    }
                },
            ),
        ));
        let (status, headers, _) = send(
            &app,
            post_request("/v1/chat/completions", "{\"input\":\"ping\"}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let id = headers
            .get("x-test-request-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let parsed = uuid::Uuid::parse_str(&id).unwrap();
        assert_eq!(parsed.get_version_num(), 7);
        let (name, _) = harness.only_log();
        let short = id.get(id.len() - 8..).unwrap();
        assert!(name.ends_with(&format!("-{short}.log")), "{name}");
        assert!(!name.contains(&id), "{name}");
    }
}

// Not upstream's: with `request-log` on, a body past the server's limit is
// kept up to the limit, and the handler still sees all of it.
#[tokio::test]
async fn bounds_the_body_read_ahead() {
    let harness = Harness::with(
        true,
        ServerConfig {
            body_limit: 16,
            ..ServerConfig::default()
        },
    );
    let app = harness.app(Router::new().route(
        "/v1/responses",
        post(|request: Request| async move {
            let body = request.into_body().collect().await.unwrap().to_bytes();
            (StatusCode::PAYLOAD_TOO_LARGE, body)
        }),
    ));
    let chunks =
        stream::iter(["0123456789", "abcdefghij", "KLMNOPQRST"].map(Ok::<_, std::io::Error>));
    let (status, _, echoed) = send(
        &app,
        post_request("/v1/responses", Body::from_stream(chunks)),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(echoed, "0123456789abcdefghijKLMNOPQRST");
    let (_, log) = harness.only_log();
    assert!(
        log.contains(
            "=== REQUEST BODY ===\n0123456789abcdef\n[REQUEST BODY TRUNCATED: captured first 16 bytes]\n\n"
        ),
        "{log}"
    );
}

// Not upstream's: with `request-log` off, the error log shows what the
// handler read of the body, and says what it didn't.
#[tokio::test]
async fn shows_what_the_handler_read() {
    let harness = Harness::new(false);
    let app = harness.app(answering(500));
    send(&app, post_request("/v1/responses", "{\"model\":\"gpt-4\"}")).await;
    let (name, log) = harness.only_log();
    assert!(name.starts_with("error-v1-responses-"), "{name}");
    assert!(
        log.contains(
            "=== REQUEST BODY ===\n[REQUEST BODY CAPTURE INCOMPLETE: consumed 0 of 17 bytes]\n\n"
        ),
        "{log}"
    );
}

// Not upstream's: a client that leaves before the answer is logged with
// status 499; one that leaves during it, with what was sent.
#[tokio::test]
async fn logs_clients_that_leave() {
    let harness = Harness::new(true);
    let app = harness.app(Router::new().route(
        "/v1/slow",
        post(|| async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            StatusCode::OK
        }),
    ));
    let call = app.clone().oneshot(post_request("/v1/slow", "{}"));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), call)
            .await
            .is_err()
    );
    let (name, log) = harness.only_log();
    assert!(name.starts_with("v1-slow-"), "{name}");
    assert!(log.contains("=== RESPONSE ===\nStatus: 499\n"), "{log}");

    let harness = Harness::new(true);
    let app = harness.app(Router::new().route(
        "/v1/partial",
        post(|| async {
            let first = stream::once(async { Ok::<_, std::io::Error>("data: 1\n\n") });
            Body::from_stream(first.chain(stream::pending()))
        }),
    ));
    let response = app
        .clone()
        .oneshot(post_request("/v1/partial", "{}"))
        .await
        .unwrap();
    let mut body = response.into_body();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(first, "data: 1\n\n");
    drop(body);
    let (_, log) = harness.only_log();
    assert!(log.contains("=== RESPONSE ===\nStatus: 200\n"), "{log}");
    assert!(log.contains("\ndata: 1\n"), "{log}");
}

// Not upstream's: a Responses WebSocket upgrade is logged, a plain GET
// isn't, and the upgrade's log waits for the session to end.
#[tokio::test]
async fn logs_websocket_upgrades() {
    let harness = Harness::new(true);
    let held: Arc<std::sync::Mutex<Option<Arc<RequestContext>>>> = Arc::default();
    let keep = Arc::clone(&held);
    let app = harness.app(
        Router::new()
            .route(
                "/v1/responses",
                get(
                    move |State(state): State<AppState>,
                          Extension(context): Extension<Arc<RequestContext>>| async move {
                        Upstream {
                            kind: AttemptKind::Websocket,
                            url: "wss://api.example.com/v1/responses",
                            chunks: &["{\"type\":\"response.completed\"}"],
                            ..Upstream::default()
                        }
                        .run(&state, &context);
                        *keep.lock().unwrap() = Some(context);
                        StatusCode::SWITCHING_PROTOCOLS
                    },
                ),
            )
            .route("/v1/models", get(|| async { "{}" })),
    );
    send(
        &app,
        Request::get("/v1/models").body(Body::empty()).unwrap(),
    )
    .await;
    let upgrade = Request::get("/v1/responses")
        .header("upgrade", "websocket")
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = send(&app, upgrade).await;
    assert_eq!(status, StatusCode::SWITCHING_PROTOCOLS);
    assert!(harness.logs().is_empty());

    held.lock().unwrap().take();
    let (name, log) = harness.only_log();
    assert!(name.starts_with("v1-responses-"), "{name}");
    assert!(log.contains("Downstream Transport: websocket\n"), "{log}");
    assert!(log.contains("=== API WEBSOCKET TIMELINE ===\n"), "{log}");
    assert!(
        log.contains("Event: api.websocket.response\n{\"type\":\"response.completed\"}"),
        "{log}"
    );
    assert!(log.contains("=== RESPONSE ===\nStatus: 101\n"), "{log}");
}
