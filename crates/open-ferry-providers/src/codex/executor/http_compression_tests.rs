use super::*;
use axum::{Router, body::Body};
use http::{HeaderValue, header};
use serde_json::json;
use std::sync::Mutex;
use tokio::io::AsyncWriteExt;

const DESTINATION: &str = "https://chatgpt.com/backend-api/codex/responses";

fn config(threshold: i64) -> Config {
    let mut config = Config::default();
    config.codex.upstream_request_compression = true;
    config.codex.upstream_request_compression_min_bytes = threshold;
    config
}
fn oauth() -> Auth {
    let mut auth = Auth {
        provider: "codex".into(),
        ..Default::default()
    };
    auth.metadata
        .insert("access_token".into(), json!("mock-oauth-token"));
    auth
}
async fn gzip(body: &[u8]) -> Vec<u8> {
    let mut writer = async_compression::tokio::write::GzipEncoder::new(Vec::new());
    writer.write_all(body).await.unwrap();
    writer.shutdown().await.unwrap();
    writer.into_inner()
}

#[tokio::test]
async fn compression_eligibility_and_threshold_family() {
    let body = Bytes::from(json!({"input":"a".repeat(4096)}).to_string());
    let headers = HeaderMap::new();
    let auth = oauth();
    for delta in [-1, 0, 1] {
        let config = config(body.len() as i64 + delta);
        let result = crate::codex::request::compress_request(
            Some(&config),
            &auth,
            DESTINATION,
            &headers,
            &body,
        )
        .await;
        assert_eq!(result.is_some(), delta <= 0);
    }
    for url in [
        "https://evil.test/backend-api/codex/responses",
        "http://chatgpt.com/backend-api/codex/responses",
        "https://chatgpt.com:444/backend-api/codex/responses",
        "https://chatgpt.com/v1/responses",
        "https://chatgpt.com.evil.test/backend-api/codex/responses",
        "https://user@chatgpt.com/backend-api/codex/responses",
    ] {
        assert!(
            crate::codex::request::compress_request(Some(&config(1)), &auth, url, &headers, &body)
                .await
                .is_none()
        );
    }
    let mut api = auth.clone();
    api.attributes.insert("api_key".into(), "mock-key".into());
    let mut siwc = auth.clone();
    siwc.metadata.insert("use_siwc".into(), json!(true));
    for excluded in [api, siwc] {
        assert!(
            crate::codex::request::compress_request(
                Some(&config(1)),
                &excluded,
                DESTINATION,
                &headers,
                &body
            )
            .await
            .is_none()
        );
    }
    assert!(
        crate::codex::request::compress_request(None, &auth, DESTINATION, &headers, &body)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn rejected_compression_retries_once_with_identical_plain_body_and_credential() {
    // Invariant: zero or one plain retry; no payload or credential changes on any status ordering.
    for first in [200, 400, 415, 429, 500] {
        for second in [200, 400, 415] {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let capture = seen.clone();
            let app = Router::new().fallback(move |headers: HeaderMap, body: Bytes| {
                let seen = capture.clone();
                async move {
                    let mut requests = seen.lock().unwrap();
                    requests.push((headers, body));
                    http::Response::builder()
                        .status(if requests.len() == 1 { first } else { second })
                        .body(Body::from("{}"))
                        .unwrap()
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut executor = CodexExecutor::new("direct").with_config(Arc::new(config(1)));
            executor.http_mock_url = Some(format!("http://{}", listener.local_addr().unwrap()));
            let server = tokio::spawn(async move { axum::serve(listener, app).await });
            let auth = oauth();
            let options = Options::new(Format::CODEX);
            let headers = build_headers(&auth, &options.headers, true).unwrap();
            let plain = Bytes::from(json!({"input":"final-rule-body".repeat(1000)}).to_string());
            let (response, _) = executor
                .send_bytes(
                    &auth,
                    DESTINATION,
                    headers,
                    plain.clone(),
                    usize::MAX,
                    Attempt::new(
                        &options,
                        AttemptKind::Execute,
                        "codex",
                        "gpt-5",
                        &Format::CODEX,
                        &auth,
                    ),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status().as_u16(),
                if matches!(first, 400 | 415) {
                    second
                } else {
                    first
                }
            );
            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), if matches!(first, 400 | 415) { 2 } else { 1 });
            assert_eq!(seen[0].0[header::CONTENT_ENCODING], "zstd");
            let decoded = crate::response_decode::decode_response_body(
                std::io::Cursor::new(seen[0].1.clone()),
                &seen[0].0,
                plain.len(),
            );
            use futures_util::TryStreamExt;
            let chunks: Vec<Bytes> = decoded.try_collect().await.unwrap();
            assert_eq!(chunks.concat(), plain);
            if seen.len() == 2 {
                assert_eq!(seen[1].1, plain);
                assert!(!seen[1].0.contains_key(header::CONTENT_ENCODING));
                assert_eq!(
                    seen[0].0[header::AUTHORIZATION],
                    seen[1].0[header::AUTHORIZATION]
                );
                assert_eq!(
                    seen[0].0[header::ACCEPT_ENCODING],
                    seen[1].0[header::ACCEPT_ENCODING]
                );
            }
            server.abort();
        }
    }
}

#[tokio::test]
async fn decoded_http_bodies_are_lazy_bounded_and_headers_match() {
    for plain in [
        b"data: response.completed\n\n".as_slice(),
        br#"{"output":[]}"#,
        br#"{"data":[{"b64_json":"aGVsbG8="}]}"#,
    ] {
        let wire = gzip(plain).await;
        let response = reqwest::Response::from(
            http::Response::builder()
                .header("content-encoding", "gzip")
                .header("content-length", wire.len())
                .body(wire.clone())
                .unwrap(),
        );
        let response = crate::codex::client::decode_response(response, plain.len());
        assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
        assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
        assert_eq!(read_body(response, plain.len()).await.unwrap(), plain);
        let response = reqwest::Response::from(
            http::Response::builder()
                .header("content-encoding", "gzip")
                .body(wire)
                .unwrap(),
        );
        assert!(
            read_body(
                crate::codex::client::decode_response(response, 1),
                plain.len()
            )
            .await
            .is_err()
        );
    }
    let (sender, receiver) =
        tokio::sync::mpsc::unbounded_channel::<Result<Bytes, std::io::Error>>();
    let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
        Some((receiver.recv().await?, receiver))
    });
    let response = reqwest::Response::from(http::Response::new(reqwest::Body::wrap_stream(stream)));
    let mut response = crate::codex::client::decode_response(response, 1024);
    sender.send(Ok(Bytes::from_static(b"x"))).unwrap();
    assert_eq!(response.chunk().await.unwrap().unwrap(), "x");
    drop(sender);
    assert!(response.chunk().await.unwrap().is_none());
    let response = reqwest::Response::from(
        http::Response::builder()
            .header("content-encoding", "gzip")
            .body("broken")
            .unwrap(),
    );
    let mut response = crate::codex::client::decode_response(response, 1024);
    assert!(response.chunk().await.is_err());
    assert!(response.chunk().await.unwrap().is_none());
}

#[test]
fn plaintext_headers_cannot_claim_compression() {
    for body in [b" {}".as_slice(), b"[1]"] {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("zstd"));
        crate::codex::request::align_content_encoding(&mut headers, body);
        assert!(!headers.contains_key(header::CONTENT_ENCODING));
    }
}

#[tokio::test]
async fn finalized_rules_and_compressed_responses_reach_executor_consumers() {
    for mode in ["normal", "stream", "compact", "image"] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let capture = seen.clone();
        let answer = if matches!(mode, "normal" | "stream") {
            b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_mock\",\"status\":\"completed\",\"output\":[]}}\n\n".to_vec()
        } else if mode == "compact" {
            br#"{"id":"resp_mock","object":"response.compaction","output":[]}"#.to_vec()
        } else {
            br#"{"created":1713833628,"data":[{"b64_json":"AA=="}]}"#.to_vec()
        };
        let wire = gzip(&answer).await;
        let app = Router::new().fallback(move |headers: HeaderMap, body: Bytes| {
            capture.lock().unwrap().push((headers, body));
            let wire = wire.clone();
            async move {
                http::Response::builder()
                    .header("content-encoding", "gzip")
                    .body(Body::from(wire))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut executor = CodexExecutor::new("direct");
        executor.http_mock_url = Some(format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let mut config = config(1);
        config.payload = serde_json::from_value(json!({"override":[{"models":[{"name":"*"}],"params":{"instructions":"rule-final-marker".repeat(1000)}}]})).unwrap();
        executor = executor.with_config(Arc::new(config));
        let mut options = Options::new(Format::OPENAI_RESPONSE);
        let model = if mode == "image" {
            "gpt-image-1.5"
        } else {
            "gpt-5"
        };
        if mode == "compact" {
            options.alt = COMPACT_ALT.into();
        }
        if mode == "image" {
            options.source_format = Format::OPENAI_IMAGE;
            options.metadata.request_path = "/v1/images/generations".into();
        }
        let request = Request {
            model: model.into(),
            payload: Bytes::from(json!({"model":model,"input":"hi","prompt":"hi"}).to_string()),
        };
        let response = if mode == "stream" {
            use futures_util::TryStreamExt;
            options.stream = true;
            let response = executor
                .execute_stream_inner(&oauth(), request, options)
                .await
                .unwrap();
            let chunks: Vec<Bytes> = response.chunks.try_collect().await.unwrap();
            Response {
                payload: Bytes::from(chunks.concat()),
                headers: response.headers,
            }
        } else {
            executor
                .execute_inner(&oauth(), &request, &options)
                .await
                .unwrap()
        };
        assert!(!response.headers.contains_key(header::CONTENT_ENCODING));
        assert!(!response.headers.contains_key(header::CONTENT_LENGTH));
        assert!(!response.payload.is_empty());
        if mode != "image" {
            let (headers, body) = seen.lock().unwrap()[0].clone();
            assert_eq!(headers[header::CONTENT_ENCODING], "zstd");
            use futures_util::TryStreamExt;
            let chunks: Vec<Bytes> = crate::response_decode::decode_response_body(
                std::io::Cursor::new(body),
                &headers,
                100_000,
            )
            .try_collect()
            .await
            .unwrap();
            let final_body: Value = serde_json::from_slice(&chunks.concat()).unwrap();
            assert_eq!(final_body["instructions"], "rule-final-marker".repeat(1000));
        }
        server.abort();
    }
}

#[test]
fn content_encoding_alignment_preserves_opaque_chains() {
    for (body, declared, content_type, expected) in [
        (
            b"--multipart".as_slice(),
            "gzip",
            "multipart/form-data; boundary=m",
            None,
        ),
        (
            &[0x28, 0xb5, 0x2f, 0xfd],
            "gzip",
            "application/octet-stream",
            Some("zstd"),
        ),
        (
            &[0x1f, 0x8b, 0, 0],
            "zstd",
            "application/octet-stream",
            Some("gzip"),
        ),
        (
            &[0x28, 0xb5, 0x2f, 0xfd],
            "gzip, zstd",
            "application/octet-stream",
            Some("gzip, zstd"),
        ),
        (
            b"opaque",
            "deflate",
            "application/octet-stream",
            Some("deflate"),
        ),
        (b"opaque", "zstd", "application/octet-stream", None),
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, content_type.parse().unwrap());
        headers.insert(header::CONTENT_ENCODING, declared.parse().unwrap());
        crate::codex::request::align_content_encoding(&mut headers, body);
        assert_eq!(
            headers
                .get(header::CONTENT_ENCODING)
                .map(|v| v.to_str().unwrap()),
            expected
        );
    }
}

async fn long_sse_mock() -> (
    CodexExecutor,
    tokio::task::JoinHandle<Result<(), std::io::Error>>,
) {
    let mut encoder = async_compression::tokio::write::GzipEncoder::new(Vec::new());
    let comment = format!(":{}\n\n", "x".repeat(65533));
    for _ in 0..=MAX_LINE / comment.len() {
        encoder.write_all(comment.as_bytes()).await.unwrap();
    }
    encoder.write_all(b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_long\",\"status\":\"completed\",\"output\":[]}}\n\n").await.unwrap();
    encoder.shutdown().await.unwrap();
    let wire = encoder.into_inner();
    let app = Router::new().fallback(move || {
        let wire = wire.clone();
        async move {
            http::Response::builder()
                .header("content-type", "text/event-stream")
                .header("content-encoding", "gzip")
                .body(Body::from(wire))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let executor = CodexExecutor::new("direct")
        .with_base_url(format!("http://{}", listener.local_addr().unwrap()));
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    (executor, server)
}

#[tokio::test]
async fn decoded_sse_above_whole_body_limit_passes() {
    use futures_util::TryStreamExt;
    let (executor, server) = long_sse_mock().await;
    let request = Request {
        model: "gpt-5".into(),
        payload: Bytes::from_static(br#"{"model":"gpt-5","input":"hi"}"#),
    };
    let auth = oauth();
    let response = executor
        .execute_inner(&auth, &request, &Options::new(Format::CODEX))
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&response.payload).contains("resp_long"));
    let mut options = Options::new(Format::CODEX);
    options.stream = true;
    let response = executor
        .execute_stream_inner(&auth, request, options)
        .await
        .unwrap();
    let chunks: Vec<Bytes> = response.chunks.try_collect().await.unwrap();
    assert!(String::from_utf8_lossy(&chunks.concat()).contains("resp_long"));
    server.abort();
}

#[tokio::test]
async fn oversized_whole_body_retains_limit_and_emits_one_error() {
    let (executor, server) = long_sse_mock().await;
    let auth = oauth();
    let options = Options::new(Format::OPENAI_RESPONSE);
    let body = json!({"input":"hi"});
    let (mut response, _) = executor
        .send(
            &auth,
            &format!("{}/responses/compact", executor.base_url),
            HeaderMap::new(),
            &body,
            MAX_LINE,
            Attempt::new(
                &options,
                AttemptKind::Execute,
                "codex",
                "gpt-5",
                &Format::CODEX,
                &auth,
            ),
        )
        .await
        .unwrap();
    let mut errors = 0;
    let mut bytes = 0;
    while let Some(chunk) = response.chunk().await.transpose() {
        match chunk {
            Ok(chunk) => bytes += chunk.len(),
            Err(_) => errors += 1,
        }
    }
    assert_eq!(errors, 1);
    assert!(bytes <= MAX_LINE);
    assert!(response.chunk().await.unwrap().is_none());
    let request = Request {
        model: "gpt-5".into(),
        payload: Bytes::from_static(br#"{"model":"gpt-5","input":"hi"}"#),
    };
    let error = executor
        .execute_compact(&auth, &request, &options)
        .await
        .unwrap_err();
    assert!(error.message.contains("limit"), "{}", error.message);
    server.abort();
}
