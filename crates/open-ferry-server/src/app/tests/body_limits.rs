// Ported from CLIProxyAPI internal/api/max_request_body_bytes_test.go (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use open_ferry_core::config::Config;
use open_ferry_core::observe::{Observability, request_log::RequestLogger};
use tower::ServiceExt;

use crate::config::{DEFAULT_BODY_LIMIT, ServerConfig};
use crate::testing::{FakeCatalog, FakeDispatcher, state};

const ROUTES: &[&str] = &[
    "/v1/chat/completions",
    "/v1/completions",
    "/v1/responses",
    "/v1/responses/compact",
    "/backend-api/codex/responses",
    "/backend-api/codex/responses/compact",
    "/v1/messages",
    "/v1/messages/count_tokens",
    "/v1beta/models/m:generateContent",
    "/v1beta/models/m:streamGenerateContent",
    "/v1beta/models/m:countTokens",
    "/v1beta/interactions",
    "/v1/images/generations",
    "/v1/images/edits",
    "/v1/audio/speech",
    "/v1/tts",
    "/v1/videos",
    "/v1/videos/generations",
    "/v1/videos/edits",
    "/v1/videos/extensions",
    "/openai/v1/videos",
    "/v1/alpha/search",
    "/backend-api/codex/alpha/search",
];

fn scratch() -> std::path::PathBuf {
    let root = std::env::var_os("OFP_TEST_SCRATCH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ofp-scratch")
        });
    let path = root.join(format!("body-limit-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn state_with_limit(limit: usize) -> crate::AppState {
    state(
        ServerConfig {
            body_limit: limit,
            api_keys: vec!["test-key".into()],
            ..ServerConfig::default()
        },
        FakeCatalog::new(),
        &FakeDispatcher::new([]),
    )
}

async fn body_text(response: axum::response::Response) -> String {
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}

#[tokio::test]
async fn unauthenticated_declared_oversize_is_rejected_without_reading() {
    let polls = Arc::new(AtomicUsize::new(0));
    let read = polls.clone();
    let body = Body::from_stream(stream::once(async move {
        read.fetch_add(1, Ordering::SeqCst);
        Ok::<_, std::io::Error>(Bytes::from_static(b"never read"))
    }));
    let request = Request::post(ROUTES[0])
        .header("content-length", "128")
        .body(body)
        .unwrap();
    let response = crate::router(state_with_limit(64))
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert_eq!(body_text(response).await, r#"{"error":"Missing API key"}"#);
}

#[tokio::test]
async fn two_byte_zero_zstd_sequences_match_ruzstd_through_request_handling() {
    use http::header;
    use std::io::Read;

    for (frame, expected) in [
        (
            &b"\x28\xb5\x2f\xfd\x00\x00\x2d\x00\x00\x10\x7b\x7d\x80\x00"[..],
            &b"{}"[..],
        ),
        (
            &b"\x28\xb5\x2f\xfd\x00\x00\x45\x00\x00\x28hello\x80\x00"[..],
            &b"hello"[..],
        ),
    ] {
        let mut decoder = ruzstd::decoding::StreamingDecoder::new(frame).unwrap();
        let mut direct = Vec::new();
        decoder.read_to_end(&mut direct).unwrap();
        assert_eq!(direct, expected);

        // Drive the production router's authentication, body admission and
        // handler decoding, and compare with that same uncompressed request.
        let app = crate::router(state_with_limit(2048));
        let request = |body: Bytes, encoded: bool| {
            let mut builder = Request::post("/v1/responses")
                .header(header::AUTHORIZATION, "Bearer test-key")
                .header(header::CONTENT_TYPE, "application/json");
            if encoded {
                builder = builder.header(header::CONTENT_ENCODING, "zstd");
            }
            builder.body(Body::from(body)).unwrap()
        };
        let plain = app
            .clone()
            .oneshot(request(Bytes::copy_from_slice(expected), false))
            .await
            .unwrap();
        let compressed = app
            .oneshot(request(Bytes::copy_from_slice(frame), true))
            .await
            .unwrap();
        assert_eq!(compressed.status(), plain.status());
        let plain_text = body_text(plain).await;
        let compressed_text = body_text(compressed).await;
        assert!(!plain_text.contains("decode zstd"));
        assert_eq!(compressed_text, plain_text, "fixture {expected:?}");

        // Assert the decoded bytes as well, through the shared request reader.
        let mut headers = http::HeaderMap::new();
        headers.insert(header::CONTENT_ENCODING, "zstd".parse().unwrap());
        assert_eq!(
            crate::body::read_decoded(&headers, Body::from(frame), 2048)
                .await
                .unwrap(),
            expected
        );
    }
    // Truncated two-/three-byte counts and every nonzero count encoding still
    // require their following bytes/modes. None may become a zero-count bypass.
    for count in [
        &b"\x80"[..],
        &b"\xff"[..],
        &b"\xff\x00"[..],
        &b"\x01"[..],
        &b"\x80\x01"[..],
        &b"\xff\x00\x00"[..],
    ] {
        let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0, 0];
        let block_size = 3 + count.len();
        frame.extend_from_slice(&(((block_size as u32) << 3) | 5).to_le_bytes()[..3]);
        frame.extend_from_slice(b"\x10{}");
        frame.extend_from_slice(count);
        let mut headers = http::HeaderMap::new();
        headers.insert(header::CONTENT_ENCODING, "zstd".parse().unwrap());
        assert_eq!(
            crate::body::read_decoded(&headers, Body::from(frame), 2048)
                .await
                .unwrap_err()
                .status(),
            400
        );
    }
}

#[tokio::test]
async fn every_body_route_rejects_chunked_oversize_in_the_upstream_shape() {
    let app = crate::router(state_with_limit(64));
    for route in ROUTES {
        let chunks = vec![Bytes::from(vec![b'a'; 32]), Bytes::from(vec![b'b'; 33])];
        let request = Request::post(*route)
            .header("authorization", "Bearer test-key")
            .header("content-type", "application/json")
            .body(Body::from_stream(
                stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>)).boxed(),
            ))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE, "{route}");
        let actual: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
        let expected = match *route {
            // sdk/api/handlers/gemini/interactions_handlers.go:95-98.
            "/v1beta/interactions" => serde_json::json!({"error": {
                "message": "http: request body too large", "type": "invalid_request_error"
            }}),
            // sdk/api/handlers/openai/openai_videos_handlers.go:585,701-705.
            "/openai/v1/videos" => {
                let id = actual["id"].as_str().unwrap();
                assert!(id.starts_with("video_"), "{actual}");
                assert_eq!(id.len(), 38);
                serde_json::json!({"id": id, "object": "video", "status": "failed",
                    "progress": 0, "model": "grok-imagine-video", "error": {
                    "code": "invalid_request_error", "message": "Invalid request: http: request body too large"
                }})
            }
            // internal/api/server_routes.go:362-366 (v8.0.42, both aliases).
            "/v1/alpha/search" | "/backend-api/codex/alpha/search" => {
                serde_json::json!({"error": "Failed to read search request"})
            }
            _ => serde_json::json!({"error": {
                "message": "Invalid request: http: request body too large", "type": "invalid_request_error"
            }}),
        };
        assert_eq!(actual, expected, "{route}");
    }
}

#[tokio::test]
async fn decoded_oversize_is_rejected_with_and_without_logging() {
    let scratch = scratch();
    for logging in [false, true] {
        let mut config = Config::default();
        config.request_log = logging;
        let observability = Observability {
            request_log: RequestLogger::new(&config, &scratch, &scratch.join("config.yaml")),
            ..Observability::default()
        };
        let app = crate::router(state_with_limit(128).with_observability(observability));
        let decoded = format!(r#"{{"model":"m","input":"{}"}}"#, "z".repeat(512));
        let compressed = ruzstd::encoding::compress_to_vec(
            decoded.as_bytes(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        assert!(compressed.len() < 128);
        for route in [
            "/v1/chat/completions",
            "/v1/completions",
            "/v1/responses",
            "/v1/responses/compact",
            "/v1/images/generations",
            "/v1/audio/speech",
            "/v1/videos",
            "/openai/v1/videos",
        ] {
            let request = Request::post(route)
                .header("authorization", "Bearer test-key")
                .header("content-encoding", "zstd")
                .header("content-type", "application/json")
                .body(Body::from(compressed.clone()))
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::PAYLOAD_TOO_LARGE,
                "{route}, logging={logging}"
            );
        }
    }
}

#[tokio::test]
async fn logging_leaves_unauthenticated_chunked_bodies_unread() {
    let scratch = scratch();
    let mut config = Config::default();
    config.request_log = true;
    let observability = Observability {
        request_log: RequestLogger::new(&config, &scratch, &scratch.join("config.yaml")),
        ..Observability::default()
    };
    let app = crate::router(state_with_limit(64).with_observability(observability));
    // Authentication wins even when Full logging is enabled. No offered
    // byte may be read or charged before a key is accepted.
    let read = Arc::new(AtomicUsize::new(0));
    let counted = read.clone();
    let chunks = stream::iter(0..1024).map(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        Ok::<_, std::io::Error>(Bytes::from_static(b"x"))
    });
    let request = Request::post(ROUTES[0])
        .header("origin", "https://caller.test")
        .body(Body::from_stream(chunks))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(read.load(Ordering::SeqCst), 0);
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
    assert_eq!(body_text(response).await, r#"{"error":"Missing API key"}"#);
}

#[tokio::test]
async fn exact_boundary_and_hot_reload_use_the_current_limit() {
    let payload = r#"{"model":"m","input":"ok"}"#;
    let state = state_with_limit(payload.len());
    let app = crate::router(state.clone());
    let request = || {
        Request::post("/v1/responses")
            .header("authorization", "Bearer test-key")
            .body(Body::from(payload))
            .unwrap()
    };
    assert_eq!(
        app.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    let mut config = state.settings().config.clone();
    config.body_limit = payload.len() - 1;
    state.set_config(config);
    assert_eq!(
        app.oneshot(request()).await.unwrap().status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[test]
fn nonpositive_limits_keep_the_default_and_maximum_does_not_overflow() {
    for value in [0, -1] {
        let mut config = Config::default();
        config.max_request_body_bytes = value;
        assert_eq!(ServerConfig::from(&config).body_limit, DEFAULT_BODY_LIMIT);
    }
    let mut config = Config::default();
    config.max_request_body_bytes = i64::MAX;
    assert_eq!(ServerConfig::from(&config).body_limit, i64::MAX as usize);
}

#[tokio::test]
async fn multipart_oversize_is_rejected_before_parsing() {
    let request = Request::post("/v1/images/edits")
        .header("authorization", "Bearer test-key")
        .header("content-type", "multipart/form-data; boundary=x")
        .body(Body::from(format!(
            "--x\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\n{}\r\n--x--\r\n",
            "p".repeat(128)
        )))
        .unwrap();
    assert_eq!(
        crate::router(state_with_limit(30))
            .oneshot(request)
            .await
            .unwrap()
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[derive(Clone, Default)]
struct AccessCapture(Arc<std::sync::Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for AccessCapture {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        self.0.lock().unwrap().push(message.0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn declared_oversize_is_access_logged_after_authentication() {
    use tracing_subscriber::layer::SubscriberExt;
    crate::test_tracing::keep_every_callsite_open();
    let capture = AccessCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _guard = tracing::subscriber::set_default(subscriber);
    let request = Request::post(ROUTES[0])
        .header("content-length", "128")
        .header("authorization", "Bearer test-key")
        .body(Body::empty())
        .unwrap();
    let response = crate::router(state_with_limit(64))
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let _ = body_text(response).await;
    let lines = capture.0.lock().unwrap();
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("413 |")
                && line.contains("POST")
                && line.contains(ROUTES[0]))
            .count(),
        1,
        "{lines:?}"
    );
}

#[tokio::test]
async fn logging_read_failures_keep_cors_headers() {
    let scratch = scratch();
    let mut config = Config::default();
    config.request_log = true;
    let observability = Observability {
        request_log: RequestLogger::new(&config, &scratch, &scratch.join("config.yaml")),
        ..Observability::default()
    };
    let app = crate::router(state_with_limit(64).with_observability(observability));
    let chunks = stream::iter([Err::<Bytes, _>(std::io::Error::other("broken body"))]);
    let request = Request::post(ROUTES[0])
        .header("authorization", "Bearer test-key")
        .body(Body::from_stream(chunks))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    // An authenticated read failure keeps CORS and the body-reader error.
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
}
