// Ported from CLIProxyAPI internal/api/max_request_body_bytes_test.go (v8.0.27, MIT).
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
async fn declared_oversize_is_rejected_without_reading_or_authentication() {
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
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert_eq!(
        body_text(response).await,
        r#"{"error":"http: request body too large"}"#
    );
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
        assert_eq!(
            body_text(response).await,
            r#"{"error":{"message":"Invalid request: http: request body too large","type":"invalid_request_error"}}"#,
            "{route}"
        );
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
async fn logging_rejects_unauthenticated_chunked_oversize() {
    let scratch = scratch();
    let mut config = Config::default();
    config.request_log = true;
    let observability = Observability {
        request_log: RequestLogger::new(&config, &scratch, &scratch.join("config.yaml")),
        ..Observability::default()
    };
    let app = crate::router(state_with_limit(64).with_observability(observability));
    let request = Request::post(ROUTES[0])
        .body(Body::from_stream(stream::iter([Ok::<_, std::io::Error>(
            Bytes::from(vec![b'x'; 65]),
        )])))
        .unwrap();
    assert_eq!(
        app.oneshot(request).await.unwrap().status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
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
