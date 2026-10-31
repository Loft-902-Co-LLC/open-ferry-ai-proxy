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
            assert!(r.headers().contains_key(header::CONTENT_LENGTH));
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
#[tokio::test]
async fn codec_lifetime_and_working_output_are_bounded() {
    for codec in ["gzip", "zstd"] {
        for fail in [false, true] {
            let source_drops = Arc::new(AtomicUsize::new(0));
            let encoder = Encoder::new(codec).unwrap();
            let lifetime = Arc::downgrade(&encoder.lifetime);
            let mut body = CompressedBody {
                source: Some(Body::new(Tracked {
                    drops: source_drops.clone(),
                    step: 0,
                    fail,
                })),
                encoder: Some(encoder),
                pending: Bytes::new(),
                trailer: None,
                done: false,
                panic: None,
            };
            while let Some(frame) = body.frame().await {
                if let Ok(frame) = frame
                    && let Some(data) = frame.data_ref()
                {
                    assert!(data.len() <= CHUNK + 1024);
                }
            }
            assert!(
                lifetime.upgrade().is_none(),
                "encoder dropped while finished body is still held"
            );
            assert_eq!(source_drops.load(Ordering::SeqCst), 1);
            assert!(body.encoder.is_none() && body.source.is_none() && body.pending.is_empty());
            assert!(
                body.frame().await.is_none(),
                "writes/polls after finish cannot emit plaintext"
            );
        }
        let mut encoder = Encoder::new(codec).unwrap();
        // Incompressible input cannot grow the output allocation with stream length.
        let mut seed = 42u32;
        for _ in 0..64 {
            let input: Vec<u8> = (0..CHUNK)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    seed as u8
                })
                .collect();
            assert!(encoder.chunk(&input).unwrap().len() <= CHUNK + 1024);
        }
    }
}
struct TinyFrames {
    count: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    data: Bytes,
}
impl Drop for TinyFrames {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl HttpBody for TinyFrames {
    type Data = Bytes;
    type Error = io::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        let calls = self.count.fetch_add(1, Ordering::SeqCst);
        assert!(
            calls <= 1,
            "a flush decides without consuming the next frame"
        );
        Poll::Ready(Some(Ok(Frame::data(self.data.clone()))))
    }
}
const FLUSH_INVARIANT: &str = "first flush decides encoding; cancellation releases source";
#[tokio::test]
async fn flushed_frames_decide_without_staging_family() {
    for codec in ["gzip", "zstd"] {
        for chunk in [1, 127, CHUNK, 65536, 65537] {
            let count = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let source = TinyFrames {
                count: count.clone(),
                drops: drops.clone(),
                data: Bytes::from(vec![b'x'; chunk]),
            };
            let mut response = response(Body::new(source), "text/plain");
            response.extensions_mut().insert(FlushFrames);
            let r = compress(response, Some(codec), 1 << 30).await;
            assert!(
                !r.headers().contains_key(header::CONTENT_ENCODING),
                "{FLUSH_INVARIANT}"
            );
            assert_eq!(count.load(Ordering::SeqCst), 1, "{FLUSH_INVARIANT}");
            let mut body = r.into_body();
            assert_eq!(
                body.frame().await.unwrap().unwrap().into_data().unwrap(),
                vec![b'x'; chunk]
            );
            drop(body);
            assert_eq!(drops.load(Ordering::SeqCst), 1, "{FLUSH_INVARIANT}");
        }
    }
}
struct PanickingBody {
    step: usize,
    drops: Arc<AtomicUsize>,
}
impl Drop for PanickingBody {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl HttpBody for PanickingBody {
    type Data = Bytes;
    type Error = io::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        self.step += 1;
        if self.step == 1 {
            Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(
                b"data: first\n\n",
            )))))
        } else {
            panic!("mock committed stream panic")
        }
    }
}
#[tokio::test]
async fn committed_panic_finishes_codec_then_resumes_and_releases_writer() {
    use futures_util::FutureExt;
    for codec in ["gzip", "zstd"] {
        let drops = Arc::new(AtomicUsize::new(0));
        let encoder = Encoder::new(codec).unwrap();
        let lifetime = Arc::downgrade(&encoder.lifetime);
        let mut body = CompressedBody {
            source: Some(Body::new(PanickingBody {
                step: 0,
                drops: drops.clone(),
            })),
            encoder: Some(encoder),
            pending: Bytes::new(),
            trailer: None,
            done: false,
            panic: None,
        };
        let mut wire = body
            .frame()
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap()
            .to_vec();
        wire.extend_from_slice(&body.frame().await.unwrap().unwrap().into_data().unwrap());
        assert!(lifetime.upgrade().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let mut r = response(wire, "text/event-stream");
        r.headers_mut()
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static(codec));
        assert_eq!(decode(r).await, b"data: first\n\n");
        assert!(
            std::panic::AssertUnwindSafe(body.frame())
                .catch_unwind()
                .await
                .is_err()
        );
        assert!(body.frame().await.is_none());
    }
}
#[tokio::test]
async fn uncommitted_handler_panic_recovers_without_compressed_data() {
    use crate::testing::{FakeCatalog, FakeDispatcher, state};
    let app = crate::router_with(
        state(
            crate::ServerConfig::default(),
            FakeCatalog::new(),
            &FakeDispatcher::new([]),
        ),
        Router::new().route(
            "/panic-test",
            get(|| async {
                panic!("mock uncommitted panic");
                #[allow(unreachable_code)]
                Response::new(Body::empty())
            }),
        ),
    );
    let r = app
        .oneshot(
            HttpRequest::builder()
                .uri("/panic-test")
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!r.headers().contains_key(header::CONTENT_ENCODING));
}
#[tokio::test]
async fn empty_type_and_encoding_headers_match_absent_values() {
    let mut r = Response::new(Body::from("x".repeat(1024)));
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(""));
    r.headers_mut()
        .insert(header::CONTENT_ENCODING, HeaderValue::from_static(""));
    let r = compress(r, Some("gzip"), 1024).await;
    assert_eq!(r.headers()[header::CONTENT_ENCODING], "gzip");
    assert_eq!(
        r.headers()[header::CONTENT_TYPE],
        "text/plain; charset=utf-8"
    );
    assert_eq!(decode(r).await, vec![b'x'; 1024]);
}

mod r2;
