// Ported from CLIProxyAPI internal/logging/gin_logger_test.go
// (TestIsAIAPIPathIncludesPublicAPIGroups, TestIsAIAPIPathIncludesImages,
// TestIsAIAPIPathIncludesCodexBackend,
// TestGinLogrusLoggerAddsRequestIDForCodexBackend,
// TestGinLogrusLoggerHealthProbeStatus) and the healthy case of
// TestHealthzAccessLogging in internal/api/server_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The access log.
//!
//! Dropped:
//! - TestGinLogrusRecoveryRepanicsErrAbortHandler and
//!   TestGinLogrusRecoveryHandlesRegularPanic, which test gin's panic
//!   recovery; the router's `CatchPanicLayer` does that here.
//! - The `home_unavailable` case of TestHealthzAccessLogging: Home isn't
//!   ported, and no probe of ours answers 503. A failed probe's line is
//!   checked by `health_probe_status`.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Request};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use futures_util::StreamExt as _;
use http::{Method, StatusCode};
use http_body_util::BodyExt;
use open_ferry_core::observe::RequestContext;
use tower::ServiceExt;
use tracing::field::{Field, Visit};
use tracing::{Level, Subscriber, span};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

use super::*;
use crate::config::ServerConfig;
use crate::testing::{FakeCatalog, FakeDispatcher, state};

/// A logged event: its level, message and request ID, the event's own or
/// that of a span it is in.
#[derive(Clone, Debug)]
struct Logged {
    level: Level,
    message: String,
    request_id: Option<String>,
}

/// A layer that keeps what is logged.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<Logged>>>);

impl Capture {
    fn take(&self) -> Vec<Logged> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// A span's request ID.
struct SpanRequestId(String);

#[derive(Default)]
struct Fields {
    message: String,
    request_id: Option<String>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_debug(field, &format_args!("{value}"));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "message" => self.message = format!("{value:?}"),
            "request_id" => self.request_id = Some(format!("{value:?}")),
            _ => {}
        }
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Capture {
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        if let (Some(request_id), Some(span)) = (fields.request_id, ctx.span(id)) {
            span.extensions_mut().insert(SpanRequestId(request_id));
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let request_id = fields.request_id.or_else(|| {
            ctx.event_scope(event)?.from_root().find_map(|span| {
                let extensions = span.extensions();
                extensions.get::<SpanRequestId>().map(|id| id.0.clone())
            })
        });
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Logged {
                level: *event.metadata().level(),
                message: fields.message,
                request_id,
            });
    }
}

/// Gives each request a context, as the router's request context layer
/// does.
async fn with_context(mut request: Request, next: Next) -> Response {
    let context = RequestContext::new(request.method().clone(), request.uri().path().to_owned());
    request.extensions_mut().insert(Arc::new(context));
    next.run(request).await
}

/// `router` behind the access log, with a request context.
fn logged(router: Router) -> Router {
    router
        .layer(middleware::from_fn(layer))
        .layer(middleware::from_fn(with_context))
}

/// Sends `request` to `app` and reads its whole answer, capturing what is
/// logged meanwhile.
async fn send(app: &Router, request: Request) -> (StatusCode, Vec<Logged>) {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _guard = tracing::subscriber::set_default(subscriber);
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let _ = response.into_body().collect().await.unwrap();
    (status, capture.take())
}

fn request(method: Method, path: &str) -> Request {
    Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

// Ports TestIsAIAPIPathIncludesPublicAPIGroups.
#[test]
fn is_ai_api_path_includes_public_api_groups() {
    for path in [
        "/v1",
        "/v1/models",
        "/v1/alpha/search",
        "/v1beta/interactions",
        "/openai/v1/videos",
        "/backend-api/codex/responses",
    ] {
        assert!(is_ai_api_path(path), "{path}");
    }
    for path in [
        "/v0/management/config",
        "/v10/models",
        "/openai/v10/videos",
        "/backend-api/codex-status",
    ] {
        assert!(!is_ai_api_path(path), "{path}");
    }
}

// Ports TestIsAIAPIPathIncludesImages.
#[test]
fn is_ai_api_path_includes_images() {
    for path in [
        "/v1/images/generations",
        "/v1/images/edits",
        "/v1/videos",
        "/v1/videos/video_123",
        "/openai/v1/videos",
        "/openai/v1/videos/video_123/content",
    ] {
        assert!(is_ai_api_path(path), "{path}");
    }
}

// Ports TestIsAIAPIPathIncludesCodexBackend.
#[test]
fn is_ai_api_path_includes_codex_backend() {
    for path in [
        "/backend-api/codex/responses",
        "/backend-api/codex/responses/compact",
    ] {
        assert!(is_ai_api_path(path), "{path}");
    }
    assert!(!is_ai_api_path("/backend-api/codex-status"));
}

// Ports TestGinLogrusLoggerAddsRequestIDForCodexBackend: what the handler
// logs, and the access line, carry the request's ID.
#[tokio::test]
async fn adds_request_id_for_codex_backend() {
    let seen = Arc::new(Mutex::new(None::<String>));
    let handler_seen = Arc::clone(&seen);
    let app = logged(Router::new().route(
        "/backend-api/codex/responses",
        any(
            move |Extension(context): Extension<Arc<RequestContext>>| async move {
                *handler_seen.lock().unwrap() = Some(context.id.to_string());
                tracing::info!("handling");
                StatusCode::OK
            },
        ),
    ));

    let (status, logged) = send(&app, request(Method::POST, "/backend-api/codex/responses")).await;
    assert_eq!(status, StatusCode::OK);
    let id = seen.lock().unwrap().clone().unwrap();
    assert_eq!(logged.len(), 2, "{logged:?}");
    assert_eq!(logged[0].message, "handling");
    assert_eq!(logged[0].request_id.as_deref(), Some(id.as_str()));
    assert_eq!(logged[1].request_id.as_deref(), Some(id.as_str()));
    assert!(
        logged[1]
            .message
            .ends_with(r#" | POST    "/backend-api/codex/responses""#),
        "{logged:?}"
    );
}

/// Logs as it is dropped.
struct Noisy;

impl Drop for Noisy {
    fn drop(&mut self) {
        tracing::info!("body dropped");
    }
}

// Not upstream's: what is logged while the answer's body is read, and as it
// is dropped, carries the request's ID, though the body is read outside the
// handler.
#[tokio::test]
async fn lines_logged_while_the_body_is_sent_carry_the_request_id() {
    let app = logged(Router::new().route(
        "/v1/stream",
        any(|| async {
            let noisy = Noisy;
            let chunks = futures_util::stream::iter(0..2).map(move |n| {
                let _kept = &noisy;
                tracing::info!("sending {n}");
                Ok::<_, std::io::Error>(format!("chunk {n}"))
            });
            Body::from_stream(chunks)
        }),
    ));

    let (status, logged) = send(&app, request(Method::GET, "/v1/stream")).await;
    assert_eq!(status, StatusCode::OK);
    let messages: Vec<_> = logged.iter().map(|line| line.message.as_str()).collect();
    assert_eq!(messages.len(), 4, "{logged:?}");
    assert_eq!(&messages[..2], ["sending 0", "sending 1"]);
    assert!(
        messages[2].ends_with(r#" | GET     "/v1/stream""#),
        "{logged:?}"
    );
    assert_eq!(messages[3], "body dropped");
    let id = logged[2].request_id.as_deref();
    assert!(id.is_some_and(|id| id.len() == 36), "{logged:?}");
    for line in &logged {
        assert_eq!(line.request_id.as_deref(), id, "{logged:?}");
    }
}

/// A handler no request should reach.
async fn unreached() -> StatusCode {
    panic!("aborted request reached route handler")
}

// Ports TestGinLogrusLoggerHealthProbeStatus: a probe answered with a 2xx
// isn't logged, even when a middleware answers it.
#[tokio::test]
async fn health_probe_status() {
    let cases = [
        ("get_ok", Method::GET, "/healthz", 200, false),
        ("head_ok", Method::HEAD, "/healthz", 200, false),
        ("success_boundary", Method::GET, "/healthz", 299, false),
        ("redirect", Method::GET, "/healthz", 300, true),
        ("client_error", Method::GET, "/healthz", 400, true),
        ("server_error", Method::HEAD, "/healthz", 503, true),
        ("similar_path", Method::GET, "/healthz-extra", 200, true),
        ("other_method", Method::POST, "/healthz", 200, true),
    ];
    for (name, method, path, status, want_log) in cases {
        let status = StatusCode::from_u16(status).unwrap();
        // A middleware answers before the route's handler runs.
        let app = logged(
            Router::new()
                .route(path, any(unreached))
                .layer(middleware::from_fn(move |_: Request, _: Next| async move {
                    status.into_response()
                })),
        );
        let (got, logged) = send(&app, request(method, path)).await;
        assert_eq!(got, status, "{name}");
        assert_eq!(!logged.is_empty(), want_log, "{name}: {logged:?}");
    }
}

// Ports the healthy case of TestHealthzAccessLogging, through the router:
// a `GET` or `HEAD` of the real `/healthz` route isn't logged, and an
// ordinary request after it is, with its ID.
#[tokio::test]
async fn healthz_probe_leaves_no_line_and_the_next_request_does() {
    let dispatcher = FakeDispatcher::new([]);
    let app = crate::router(state(
        ServerConfig::default(),
        FakeCatalog::new(),
        &dispatcher,
    ));
    for method in [Method::GET, Method::HEAD] {
        let (status, logged) = send(&app, request(method.clone(), "/healthz")).await;
        assert_eq!(status, StatusCode::OK, "{method}");
        assert!(logged.is_empty(), "{method}: {logged:?}");

        let control = "/healthz-access-log-control";
        let (_, logged) = send(&app, request(Method::GET, control)).await;
        assert!(
            logged.iter().any(|line| {
                line.request_id.is_some() && line.message.contains(&format!("\"{control}\""))
            }),
            "{method}: no access line after the probe: {logged:?}"
        );
    }
}

/// Not upstream's: the line's columns, its level following the status, the
/// query with its key masked, and no request ID off the AI routes.
#[tokio::test]
async fn writes_upstreams_line() {
    let app = logged(
        Router::new()
            .route("/v1/models", any(|| async { StatusCode::BAD_REQUEST }))
            .route(
                "/v0/management/config",
                any(|| async { StatusCode::SERVICE_UNAVAILABLE }),
            ),
    );

    let (_, logged) = send(
        &app,
        request(
            Method::GET,
            "/v1/models?key=AIzaSyA1234567890abcdef&alt=sse",
        ),
    )
    .await;
    assert_eq!(logged.len(), 1, "{logged:?}");
    let line = &logged[0];
    assert_eq!(line.level, Level::WARN);
    assert!(
        line.request_id.as_deref().is_some_and(|id| id.len() == 36),
        "{line:?}"
    );
    let message = line.message.as_str();
    assert!(message.starts_with("400 | "), "{message}");
    assert!(
        message.ends_with(r#" |                 | GET     "/v1/models?key=AIza...cdef&alt=sse""#),
        "{message}"
    );
    assert!(!message.contains("AIzaSyA1234567890abcdef"), "{message}");

    let (_, logged) = send(&app, request(Method::PUT, "/v0/management/config")).await;
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert_eq!(logged[0].level, Level::ERROR);
    assert_eq!(logged[0].request_id.as_deref(), Some(NO_REQUEST_ID));
}

/// Not upstream's: a key of one or two bytes, which upstream writes as it
/// is, is hidden whole.
#[tokio::test]
async fn hides_short_keys_whole() {
    let app = logged(Router::new().route("/v1/models", any(|| async { StatusCode::OK })));
    let (_, logged) = send(&app, request(Method::GET, "/v1/models?key=xy&alt=sse")).await;
    assert_eq!(logged.len(), 1, "{logged:?}");
    let message = logged[0].message.as_str();
    assert!(
        message.ends_with(r#" | GET     "/v1/models?key=...&alt=sse""#),
        "{message}"
    );
}

/// Not upstream's: an OAuth callback's code and state are masked, their
/// names kept; upstream's line holds them as they came.
#[tokio::test]
async fn masks_an_oauth_callbacks_code_and_state() {
    let app = logged(Router::new().route("/anthropic/callback", any(|| async { StatusCode::OK })));
    let code = "ac_0123456789abcdefghij";
    let state = "st_9876543210zyxwvuts";
    let (_, logged) = send(
        &app,
        request(
            Method::GET,
            &format!("/anthropic/callback?code={code}&state={state}&scope=user"),
        ),
    )
    .await;
    assert_eq!(logged.len(), 1, "{logged:?}");
    let message = logged[0].message.as_str();
    assert!(
        message.ends_with(
            r#" | GET     "/anthropic/callback?code=ac_0...ghij&state=st_9...vuts&scope=user""#
        ),
        "{message}"
    );
    assert!(!message.contains(code), "{message}");
    assert!(!message.contains(state), "{message}");
}

/// Not upstream's: an auth file's name in a management route's `?name=`,
/// its `@` encoded or not, and an email in the path are masked; upstream's
/// line holds them as they came.
#[tokio::test]
async fn masks_emails_in_the_path_and_query() {
    let app = logged(
        Router::new()
            .route(
                "/v0/management/auth-files/download",
                any(|| async { StatusCode::OK }),
            )
            .route("/users/{email}", any(|| async { StatusCode::OK })),
    );
    for (uri, want) in [
        (
            "/v0/management/auth-files/download?name=codex-1a2b3c4d-john.doe%40example.com-plus.json",
            r#" | GET     "/v0/management/auth-files/download?name=codex-1a2b3c4d-j***%40e***.com-plus.json""#,
        ),
        (
            "/v0/management/auth-files/download?name=claude-john.doe@example.com.json&key=AIza0123456789",
            r#" | GET     "/v0/management/auth-files/download?name=claude-j***@e***.com.json&key=AIza...6789""#,
        ),
        (
            "/users/john.doe@example.com",
            r#" | GET     "/users/j***@e***.com""#,
        ),
    ] {
        let (_, logged) = send(&app, request(Method::GET, uri)).await;
        assert_eq!(logged.len(), 1, "{logged:?}");
        let message = logged[0].message.as_str();
        assert!(message.ends_with(want), "{message}");
        assert!(!message.contains("john.doe"), "{message}");
        assert!(!message.contains("example.com"), "{message}");
    }
}

/// Not upstream's: the time taken, as gin writes it.
#[test]
fn latency_prints_as_go_does() {
    let cases = [
        (Duration::ZERO, "0s"),
        (Duration::from_micros(999), "0s"),
        (Duration::from_micros(23_559), "23ms"),
        (Duration::from_millis(1000), "1s"),
        (Duration::from_millis(23_559), "23.559s"),
        (Duration::from_millis(1_050), "1.05s"),
        (Duration::from_millis(60_000), "1m0s"),
        (Duration::from_millis(65_999), "1m5s"),
        (Duration::from_secs(3600), "1h0m0s"),
        (Duration::from_secs(3723), "1h2m3s"),
    ];
    for (elapsed, want) in cases {
        assert_eq!(go_latency(elapsed), want, "{elapsed:?}");
    }
}
