//! Ports CLIProxyAPI internal/api/middleware/response_writer_test.go
//! (v8.0.20, MIT).
//!
//! Upstream finalizes a hand-built `ResponseWriterWrapper` with a spy
//! logger; here a request goes through the capture layer to a logger
//! writing files.
//!
//! Changed:
//! - `TestHasActionableError`'s canceled context is the answer's
//!   `canceled` flag, and `context.Canceled` an error marked canceled.
//! - `TestFinalizeStreamingWritesAPIWebsocketTimeline`'s timeline comes
//!   from an upstream WebSocket attempt rather than a context value.
//!
//! Dropped, as the port has no body overrides (upstream's
//! `REQUEST_BODY_OVERRIDE`, `RESPONSE_BODY_OVERRIDE` and
//! `API_WEBSOCKET_TIMELINE` context values): the bodies logged are what was
//! sent, and the timeline is what the request's taps saw.
//! - `TestExtractRequestBodyPrefersOverride`
//! - `TestExtractRequestBodySupportsStringOverride`
//! - `TestExtractResponseBodyPrefersOverride`
//! - `TestExtractResponseBodySupportsStringOverride`
//! - `TestExtractBodyOverrideClonesBytes`
//! - `TestExtractWebsocketTimelineUsesOverride`

use std::sync::Arc;

use axum::Router;
use axum::extract::{Extension, State};
use axum::routing::post;
use http::StatusCode;
use open_ferry_core::observe::request_log::{ApiError, has_actionable_error};
use open_ferry_core::observe::{AttemptKind, RequestContext};

use super::{Harness, Upstream, post as post_request, send};
use crate::state::AppState;

// Ports TestFinalizeStreamingWritesAPIWebsocketTimeline.
#[tokio::test]
async fn finalize_streaming_writes_api_websocket_timeline() {
    let harness = Harness::new(true);
    let app =
        harness.app(Router::new().route(
            "/v1/responses",
            post(
                |State(state): State<AppState>,
                 Extension(context): Extension<Arc<RequestContext>>| async move {
                    Upstream {
                        kind: AttemptKind::Websocket,
                        url: "wss://api.example.com/v1/responses",
                        body: "{}",
                        chunks: &["{\"type\":\"response.completed\"}"],
                        ..Upstream::default()
                    }
                    .run(&state, &context);
                    (
                        [("content-type", "text/event-stream")],
                        "data: {\"type\":\"response.completed\"}\n\n",
                    )
                },
            ),
        ));
    let (status, _, _) = send(&app, post_request("/v1/responses", "{\"stream\":true}")).await;
    assert_eq!(status, StatusCode::OK);
    let (_, log) = harness.only_log();
    assert!(log.contains("Upstream Transport: websocket\n"), "{log}");
    assert!(
        log.contains("=== API WEBSOCKET TIMELINE ===\nTimestamp: "),
        "{log}"
    );
    assert!(log.contains("Event: api.websocket.request\n"), "{log}");
    assert!(log.contains("\nBody:\n{}\n"), "{log}");
    assert!(log.contains("Event: api.websocket.response\n"), "{log}");
}

fn api_error(status: u16, message: &str, canceled: bool) -> ApiError {
    ApiError {
        status,
        message: message.to_owned(),
        canceled,
    }
}

// Ports TestHasActionableError.
#[test]
fn has_actionable_error_table() {
    let cases: [(&str, u16, bool, Vec<ApiError>, bool); 11] = [
        ("200 ok without errors", 200, false, vec![], false),
        ("499 client closed request", 499, false, vec![], false),
        (
            "499 with context canceled api error",
            499,
            false,
            vec![api_error(499, "context canceled", true)],
            false,
        ),
        ("200 with canceled context", 200, true, vec![], false),
        ("0 with canceled context", 0, true, vec![], false),
        ("400 bad request", 400, false, vec![], true),
        ("429 rate limit", 429, false, vec![], true),
        ("500 internal server error", 500, false, vec![], true),
        ("503 with canceled context", 503, true, vec![], true),
        (
            "200 with actionable upstream api error",
            200,
            false,
            vec![api_error(502, "upstream failed", false)],
            true,
        ),
        (
            "200 with non-actionable cancellation api error",
            200,
            false,
            vec![api_error(0, "read: context canceled", false)],
            false,
        ),
    ];
    for (name, status, canceled, errors, want) in cases {
        assert_eq!(
            has_actionable_error(status, canceled, &errors),
            want,
            "{name}"
        );
    }
}

// Ports TestFinalizeExcludes499FromForceLog.
#[tokio::test]
async fn finalize_excludes_499_from_force_log() {
    let harness = Harness::new(false);
    let app = harness.app(Router::new().route(
        "/v1/responses",
        post(|| async { StatusCode::from_u16(499).unwrap() }),
    ));
    let (status, _, _) = send(&app, post_request("/v1/responses", "{}")).await;
    assert_eq!(status.as_u16(), 499);
    assert!(harness.logs().is_empty());
}

// Ports TestFinalizeIncludes500InForceLog.
#[tokio::test]
async fn finalize_includes_500_in_force_log() {
    let harness = Harness::new(false);
    let app = harness.app(Router::new().route(
        "/v1/responses",
        post(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
    ));
    send(&app, post_request("/v1/responses", "{}")).await;
    let (name, log) = harness.only_log();
    assert!(name.starts_with("error-v1-responses-"), "{name}");
    assert!(log.contains("=== RESPONSE ===\nStatus: 500\n"), "{log}");
}
