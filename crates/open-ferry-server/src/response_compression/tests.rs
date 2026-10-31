use super::*;
use axum::{Router, routing::get};
use http::{Request as HttpRequest, StatusCode};
use std::{
    io::Read,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tower::ServiceExt;

fn response(data: impl Into<Body>, media: &str) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, media)
        .body(data.into())
        .unwrap()
}
async fn decode(response: Response) -> Vec<u8> {
    let encoding = response
        .headers()
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let mut out = Vec::new();
    match encoding.as_str() {
        "gzip" => {
            flate2::read::MultiGzDecoder::new(&bytes[..])
                .read_to_end(&mut out)
                .unwrap();
        }
        "zstd" => {
            ruzstd::decoding::StreamingDecoder::new(&bytes[..])
                .unwrap()
                .read_to_end(&mut out)
                .unwrap();
        }
        _ => out.extend_from_slice(&bytes),
    }
    out
}
#[test]
fn upstream_negotiation_cases() {
    for (accept, expected, identity) in [
        ("", None, true),
        ("identity", None, true),
        ("gzip;q=0", None, true),
        ("gzip;q=1,zstd;q=0.5", Some("gzip"), true),
        ("gzip;q=.5,zstd;q=.5", Some("zstd"), true),
        ("gzip,identity;q=0", Some("gzip"), false),
        ("gzip,*;q=0", Some("gzip"), false),
        ("gzip,*;q=0,identity;q=1", Some("gzip"), true),
        ("identity;q=0,*;q=0", None, false),
        ("*;q=.5", Some("zstd"), true),
        ("*;q=1,zstd;q=0", Some("gzip"), true),
        ("*;q=1,zstd;q=0,gzip;q=0", None, true),
        ("zstd;q=invalid,gzip;q=2", None, true),
        ("zstd;q=NaN,gzip;q=0.5", Some("gzip"), true),
        ("gzip;q=1,gzip;q=0", None, true),
    ] {
        assert_eq!(negotiate(accept), (expected, identity), "{accept}");
    }
}
#[tokio::test]
async fn thresholds_exclusions_headers_and_codecs() {
    for codec in ["gzip", "zstd"] {
        for size in [0, 31, 1023, 1024, 65536] {
            let original = "x".repeat(size);
            let mut r = response(original.clone(), "application/json");
            r.headers_mut()
                .insert(header::VARY, HeaderValue::from_static("Origin, origin"));
            r.headers_mut()
                .append(header::VARY, HeaderValue::from_static("ACCEPT-ENCODING"));
            r.headers_mut()
                .insert(header::CONTENT_LENGTH, size.to_string().parse().unwrap());
            let r = compress(r, Some(codec), 1024).await;
            assert_eq!(
                r.headers().contains_key(header::CONTENT_ENCODING),
                size >= 1024
            );
            assert_eq!(r.headers()[header::VARY], "Origin, ACCEPT-ENCODING");
            assert_eq!(
                r.headers().contains_key(header::CONTENT_LENGTH),
                size < 1024
            );
            assert_eq!(decode(r).await, original.as_bytes());
        }
        for media in [
            "application/json",
            "text/plain",
            "application/problem+json",
            "application/javascript",
            "image/svg+xml",
        ] {
            let r = compress(response("x".repeat(1024), media), Some(codec), 1024).await;
            assert_eq!(r.headers()[header::CONTENT_ENCODING], codec);
            assert_eq!(decode(r).await.len(), 1024);
        }
        for status in [101, 204, 206, 304] {
            let mut r = response("abc", "text/event-stream");
            *r.status_mut() = StatusCode::from_u16(status).unwrap();
            let r = compress(r, Some(codec), 0).await;
            assert!(!r.headers().contains_key(header::CONTENT_ENCODING));
            assert_eq!(r.headers()[header::VARY], "Accept-Encoding");
        }
        let mut r = response("encoded", "application/json");
        r.headers_mut()
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static("br"));
        assert_eq!(
            compress(r, Some(codec), 0).await.headers()[header::CONTENT_ENCODING],
            "br"
        );
        assert!(
            !compress(response("abc", "image/png"), Some(codec), 0)
                .await
                .headers()
                .contains_key(header::CONTENT_ENCODING)
        );
        let r = compress(response("{}", "application/json"), Some(codec), 0).await;
        assert_eq!(r.headers()[header::CONTENT_ENCODING], codec);
        assert_eq!(decode(r).await, b"{}");
    }
}
#[tokio::test]
async fn listener_reload_and_bypass() {
    use crate::testing::{FakeCatalog, FakeDispatcher, state};
    let mut core = open_ferry_core::config::Config::default();
    let state = state(
        crate::ServerConfig::from(&core),
        FakeCatalog::new(),
        &FakeDispatcher::new([]),
    );
    let app = crate::router_with(
        state.clone(),
        Router::new().route(
            "/compression-test",
            get(|| async { response("x".repeat(1024), "application/json") }),
        ),
    );
    for (enabled, threshold, expected) in [
        (true, 1024, true),
        (false, 1024, false),
        (true, 2048, false),
        (true, 1, true),
    ] {
        core.response_compression.enabled = Some(enabled);
        core.response_compression.min_bytes = threshold;
        state.set_config(crate::ServerConfig::from(&core));
        let request = HttpRequest::builder()
            .uri("/compression-test")
            .header(header::ACCEPT_ENCODING, "gzip")
            .body(Body::empty())
            .unwrap();
        let r = app.clone().oneshot(request).await.unwrap();
        assert_eq!(r.headers().contains_key(header::CONTENT_ENCODING), expected);
        assert_eq!(r.headers().contains_key(header::VARY), enabled);
        assert_eq!(decode(r).await.len(), 1024);
    }
    for (method, upgrade, vary) in [(Method::HEAD, false, true), (Method::GET, true, false)] {
        let mut request = HttpRequest::builder()
            .method(method)
            .uri("/compression-test")
            .header(header::ACCEPT_ENCODING, "gzip");
        if upgrade {
            request = request.header(header::UPGRADE, "websocket");
        }
        let r = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(!r.headers().contains_key(header::CONTENT_ENCODING));
        assert_eq!(r.headers().contains_key(header::VARY), vary);
    }
}
struct Tracked {
    drops: Arc<AtomicUsize>,
    step: usize,
    fail: bool,
}
impl Drop for Tracked {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl HttpBody for Tracked {
    type Data = Bytes;
    type Error = io::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        self.step += 1;
        Poll::Ready(match self.step {
            1 => Some(Ok(Frame::data(Bytes::from_static(b"data: first\n\n")))),
            2 if self.fail => Some(Err(io::Error::other("client close"))),
            2 => Some(Ok(Frame::trailers(HeaderMap::from_iter([(
                header::HeaderName::from_static("x-end"),
                HeaderValue::from_static("done"),
            )])))),
            _ => None,
        })
    }
}
#[tokio::test]
async fn sse_flush_trailers_error_and_drop_release_resources() {
    for codec in ["gzip", "zstd"] {
        for fail in [false, true] {
            for cancel in [false, true] {
                let drops = Arc::new(AtomicUsize::new(0));
                let source = Body::new(Tracked {
                    drops: drops.clone(),
                    step: 0,
                    fail,
                });
                let mut body = compress(
                    response(source, "text/event-stream"),
                    Some(codec),
                    usize::MAX,
                )
                .await
                .into_body();
                let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
                assert!(!first.is_empty());
                // First event is decodable before completion or trailers.
                let mut plain = [0; 13];
                if codec == "gzip" {
                    flate2::read::GzDecoder::new(&first[..])
                        .read_exact(&mut plain)
                        .unwrap();
                } else {
                    zstd::stream::read::Decoder::new(&first[..])
                        .unwrap()
                        .read_exact(&mut plain)
                        .unwrap();
                }
                assert_eq!(&plain, b"data: first\n\n");
                if !cancel {
                    if fail {
                        assert!(body.frame().await.unwrap().is_err());
                        assert!(body.frame().await.is_none());
                    } else {
                        assert!(body.frame().await.unwrap().unwrap().is_data());
                        assert_eq!(
                            drops.load(Ordering::SeqCst),
                            1,
                            "source released before trailers"
                        );
                        assert!(body.frame().await.unwrap().unwrap().is_trailers());
                        assert!(body.frame().await.is_none());
                    }
                }
                drop(body);
                assert_eq!(drops.load(Ordering::SeqCst), 1);
            }
        }
    }
}
#[test]
fn zstd_window_is_one_mib() {
    let mut e = Encoder::new("zstd").unwrap();
    let head = e.chunk(b"data: first\n\n").unwrap();
    assert_eq!(&head[..4], &[0x28, 0xb5, 0x2f, 0xfd]);
    assert_eq!(head[4] & 32, 0, "streaming frame has a window descriptor");
    assert_eq!(head[5], (20 - 10) << 3, "1 MiB window descriptor");
}
#[tokio::test]
async fn untyped_response_uses_go_content_sniffing() {
    for (plain, compressed) in [(vec![b'x'; 1024], true), (vec![0; 1024], false)] {
        let r = compress(Response::new(Body::from(plain.clone())), Some("gzip"), 1024).await;
        assert_eq!(
            r.headers().contains_key(header::CONTENT_ENCODING),
            compressed
        );
        assert_eq!(decode(r).await, plain);
    }
}
