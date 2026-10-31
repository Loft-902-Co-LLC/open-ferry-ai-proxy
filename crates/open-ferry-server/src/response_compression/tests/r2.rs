use super::*;
use crate::testing::{FakeCatalog, FakeDispatcher, Outcome, state};
use futures_util::{StreamExt, future::BoxFuture};
use open_ferry_core::exec::{
    Dispatcher, ExecError, Options, ProviderId, Request as ExecRequest, Response as ExecResponse,
    StreamResponse,
};
use std::time::Duration;

fn request(uri: &str, body: &str) -> HttpRequest<Body> {
    HttpRequest::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT_ENCODING, "gzip")
        .body(Body::from(body.to_owned()))
        .unwrap()
}
#[tokio::test(start_paused = true)]
async fn r2_keepalives_force_identity_at_first_flush() {
    let config = crate::ServerConfig {
        nonstream_keepalive: Some(Duration::from_secs(5)),
        ..Default::default()
    };
    let payload = Bytes::from(format!(
        "{{\"choices\":[{{\"message\":{{\"content\":\"{}\"}}}}]}}",
        "x".repeat(2000)
    ));
    let dispatcher = FakeDispatcher::new([Outcome::Slow(
        Duration::from_secs(90),
        ExecResponse {
            payload,
            headers: HeaderMap::new(),
        },
    )]);
    let app = crate::router(state(
        config,
        FakeCatalog::new().serve("gpt", &["codex"]),
        &dispatcher,
    ));
    let start = tokio::time::Instant::now();
    let r = tokio::time::timeout(
        Duration::from_secs(6),
        app.oneshot(request(
            "/v1/chat/completions",
            r#"{"model":"gpt","messages":[],"stream":false}"#,
        )),
    )
    .await
    .expect("headers and first keepalive must arrive before the answer")
    .unwrap();
    assert!(!r.headers().contains_key(header::CONTENT_ENCODING));
    assert_eq!(start.elapsed(), Duration::from_secs(5));
    let mut body = r.into_body();
    for tick in 1..=17 {
        assert_eq!(
            body.frame().await.unwrap().unwrap().into_data().unwrap(),
            "\n"
        );
        assert_eq!(start.elapsed(), Duration::from_secs(5 * tick));
    }
    assert!(
        body.frame()
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap()
            .len()
            > 1024
    );
    assert_eq!(start.elapsed(), Duration::from_secs(90));
    assert!(body.frame().await.is_none());
}
struct TimedGemini;
impl Dispatcher for TimedGemini {
    fn execute<'a>(
        &'a self,
        _: &'a [ProviderId],
        _: ExecRequest,
        _: Options,
    ) -> BoxFuture<'a, Result<ExecResponse, ExecError>> {
        Box::pin(async { panic!("not used") })
    }
    fn count_tokens<'a>(
        &'a self,
        _: &'a [ProviderId],
        _: ExecRequest,
        _: Options,
    ) -> BoxFuture<'a, Result<ExecResponse, ExecError>> {
        Box::pin(async { panic!("not used") })
    }
    fn execute_stream<'a>(
        &'a self,
        _: &'a [ProviderId],
        _: ExecRequest,
        _: Options,
    ) -> BoxFuture<'a, Result<StreamResponse, ExecError>> {
        Box::pin(async {
            let chunks = futures_util::stream::unfold(0u8, |n| async move {
                if n == 5 {
                    return None;
                }
                if n > 0 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Some((Ok(Bytes::from(vec![b'a' + n; 300])), n + 1))
            })
            .boxed();
            Ok(StreamResponse {
                headers: HeaderMap::new(),
                chunks,
            })
        })
    }
}
#[tokio::test(start_paused = true)]
async fn r2_gemini_json_flushes_each_chunk() {
    let state = AppState::new(
        Default::default(),
        Arc::new(TimedGemini),
        Arc::new(FakeCatalog::new().serve("gemini", &["gemini"])),
    );
    let app = crate::router(state);
    let start = tokio::time::Instant::now();
    let r = tokio::time::timeout(
        Duration::from_millis(500),
        app.oneshot(request(
            "/v1beta/models/gemini:streamGenerateContent?alt=json",
            r#"{"contents":[]}"#,
        )),
    )
    .await
    .expect("first chunk must commit immediately")
    .unwrap();
    assert!(!r.headers().contains_key(header::CONTENT_ENCODING));
    let mut body = r.into_body();
    for n in 0..5u8 {
        assert_eq!(
            body.frame().await.unwrap().unwrap().into_data().unwrap(),
            vec![b'a' + n; 300]
        );
        assert_eq!(start.elapsed(), Duration::from_secs(u64::from(n)));
    }
    assert!(body.frame().await.is_none());
}
#[tokio::test]
async fn r2_fixed_count_tokens_keeps_length() {
    let dispatcher = FakeDispatcher::new([Outcome::reply(r#"{"input_tokens":3}"#)]);
    let app = crate::router(state(
        Default::default(),
        FakeCatalog::new().serve("claude", &["claude"]),
        &dispatcher,
    ));
    let r = app
        .oneshot(request(
            "/v1/messages/count_tokens",
            r#"{"model":"claude"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(r.headers().get(header::CONTENT_LENGTH).unwrap(), "18");
    assert_eq!(decode(r).await, br#"{"input_tokens":3}"#);
}
#[tokio::test]
async fn r2_fixed_large_compressed_length() {
    for typed in [true, false] {
        let mut r = Response::new(Body::from("x".repeat(4096)));
        if typed {
            r.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        }
        let r = compress(r, Some("gzip"), 1024).await;
        let length: usize = r
            .headers()
            .get(header::CONTENT_LENGTH)
            .expect("fixed compressed length")
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(r.body().size_hint().exact(), Some(length as u64));
        assert_eq!(decode(r).await, vec![b'x'; 4096]);
    }
}
#[tokio::test]
async fn r2_management_listener_compresses() {
    let app = crate::router_without_proxy(
        state(
            Default::default(),
            FakeCatalog::new(),
            &FakeDispatcher::new([]),
        ),
        Router::new().route(
            "/v0/management/config",
            get(|| async { response("x".repeat(2048), "application/json") }),
        ),
    );
    let r = app
        .oneshot(
            HttpRequest::builder()
                .uri("/v0/management/config")
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.headers().get(header::CONTENT_ENCODING).unwrap(), "gzip");
    assert!(
        r.headers()[header::VARY]
            .to_str()
            .unwrap()
            .contains("Accept-Encoding")
    );
    assert_eq!(decode(r).await.len(), 2048);
}
#[tokio::test]
async fn r2_accept_obs_text_preserves_valid_coding() {
    let app = crate::router_with(
        state(
            Default::default(),
            FakeCatalog::new(),
            &FakeDispatcher::new([]),
        ),
        Router::new().route(
            "/obs",
            get(|| async { response("x".repeat(2048), "text/plain") }),
        ),
    );
    let r = app
        .oneshot(
            HttpRequest::builder()
                .uri("/obs")
                .header(
                    header::ACCEPT_ENCODING,
                    HeaderValue::from_bytes(b"gzip, x-\xe9").unwrap(),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.headers().get(header::CONTENT_ENCODING).unwrap(), "gzip");
}
#[test]
fn r2_accept_go_hex_qualities() {
    for value in ["0x1p-1", "0X1P-1", "0x_1p-1", "0x1.0p-1", "0_0.5"] {
        assert_eq!(
            negotiate(&format!("gzip;q={value}")),
            (Some("gzip"), true),
            "{value}"
        );
    }
}
