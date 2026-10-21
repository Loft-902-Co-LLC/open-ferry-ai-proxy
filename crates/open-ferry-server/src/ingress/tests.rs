use super::*;
use axum::routing::post;
use futures_util::stream;
use http_body_util::BodyExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;
use tower::ServiceExt;

fn ingress(limit: usize) -> Ingress {
    Ingress::new(
        IngressConfig {
            buffered_body_bytes: Some(limit),
            body_idle_timeout: Some(Duration::from_secs(5)),
            body_total_timeout: Some(Duration::from_secs(20)),
            ..IngressConfig::default()
        },
        limit,
    )
}

fn app(ingress: &Ingress) -> Router {
    ingress.wrap(Router::new().route("/", post(|bytes: Bytes| async move { bytes })))
}

fn stalled(started: Arc<Notify>) -> Body {
    Body::from_stream(
        stream::once(async move {
            started.notify_one();
            Ok::<_, std::io::Error>(Bytes::from_static(b"ab"))
        })
        .chain(stream::pending()),
    )
}

async fn settle() {
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn stalled_upload_gets_408_and_releases_its_declared_reservation() {
    let ingress = ingress(8);
    let started = Arc::new(Notify::new());
    let request = Request::post("/")
        .header(header::CONTENT_LENGTH, "8")
        .body(stalled(started.clone()))
        .unwrap();
    let task = tokio::spawn(app(&ingress).oneshot(request));
    started.notified().await;
    assert_eq!(ingress.budget.used(), 8);
    tokio::time::advance(Duration::from_secs(6)).await;
    settle().await;
    assert!(task.is_finished(), "the body progress guard did not fire");
    let response = task.await.unwrap().unwrap();
    assert_eq!(response.status(), 408);
    assert_eq!(response.headers()[header::CONNECTION], "close");
    assert_eq!(ingress.budget.used(), 0);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error"],
        "request body read timed out"
    );
}

#[tokio::test(start_paused = true)]
async fn concurrent_declared_bodies_reserve_before_read_and_never_exceed_the_budget() {
    let ingress = ingress(12);
    let mut tasks = Vec::new();
    for _ in 0..3 {
        let started = Arc::new(Notify::new());
        tasks.push(tokio::spawn(
            app(&ingress).oneshot(
                Request::post("/")
                    .header(header::CONTENT_LENGTH, "4")
                    .body(stalled(started.clone()))
                    .unwrap(),
            ),
        ));
        started.notified().await;
    }
    assert_eq!(ingress.budget.used(), 12);
    let polled = Arc::new(AtomicUsize::new(0));
    let probe = polled.clone();
    let body = Body::from_stream(stream::once(async move {
        probe.fetch_add(1, Ordering::SeqCst);
        Ok::<_, std::io::Error>(Bytes::from_static(b"four"))
    }));
    let response = app(&ingress)
        .oneshot(
            Request::post("/")
                .header(header::CONTENT_LENGTH, "4")
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(response.headers()[header::RETRY_AFTER], "1");
    assert_eq!(polled.load(Ordering::SeqCst), 0, "read an unreserved body");
    assert!(ingress.budget.0.lock().unwrap().peak <= 12);
    for task in tasks {
        task.abort();
        let _ = task.await;
    }
    assert_eq!(
        ingress.budget.used(),
        0,
        "cancellation leaked a reservation"
    );
}

#[tokio::test(start_paused = true)]
async fn chunked_budget_exhaustion_and_disconnect_release_every_byte() {
    let ingress = ingress(8);
    let started = Arc::new(Notify::new());
    let held = tokio::spawn(
        app(&ingress).oneshot(
            Request::post("/")
                .header(header::CONTENT_LENGTH, "6")
                .body(stalled(started.clone()))
                .unwrap(),
        ),
    );
    started.notified().await;
    let response = app(&ingress)
        .oneshot(
            Request::post("/")
                .body(Body::from_stream(stream::iter([Ok::<_, std::io::Error>(
                    Bytes::from_static(b"abc"),
                )])))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(ingress.budget.used(), 6);
    held.abort();
    let _ = held.await;
    assert_eq!(ingress.budget.used(), 0);
    let broken = stream::iter([
        Ok(Bytes::from_static(b"a")),
        Err(std::io::Error::other("disconnect")),
    ]);
    let response = app(&ingress)
        .oneshot(Request::post("/").body(Body::from_stream(broken)).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(ingress.budget.used(), 0);
}

#[tokio::test]
async fn owners_clones_success_and_panic_all_release_reservations() {
    let ingress = ingress(8);
    let response = app(&ingress)
        .oneshot(Request::post("/").body(Body::from("abcd")).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(ingress.budget.used(), 4);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let clone = bytes.clone();
    drop(bytes);
    assert_eq!(ingress.budget.used(), 4);
    drop(clone);
    assert_eq!(ingress.budget.used(), 0);

    let router = ingress.wrap(Router::new().route(
        "/",
        post(|_: Bytes| async {
            panic!("test panic");
            #[allow(unreachable_code)]
            ""
        }),
    ));
    let task = tokio::spawn(router.oneshot(Request::post("/").body(Body::from("abcd")).unwrap()));
    assert!(task.await.unwrap_err().is_panic());
    assert_eq!(ingress.budget.used(), 0);
}

#[tokio::test(start_paused = true)]
async fn trickled_body_still_has_a_total_deadline() {
    let ingress = ingress(100);
    let started = Arc::new(Notify::new());
    let signal = started.clone();
    let body = Body::from_stream(stream::unfold(0, move |n| {
        let signal = signal.clone();
        async move {
            if n == 0 {
                signal.notify_one();
            } else {
                tokio::time::sleep(Duration::from_secs(4)).await;
            }
            Some((Ok::<_, std::io::Error>(Bytes::from_static(b"a")), n + 1))
        }
    }));
    let task = tokio::spawn(app(&ingress).oneshot(Request::post("/").body(body).unwrap()));
    started.notified().await;
    for _ in 0..5 {
        tokio::time::advance(Duration::from_secs(4)).await;
        settle().await;
    }
    assert!(task.is_finished());
    assert_eq!(task.await.unwrap().unwrap().status(), 408);
    assert_eq!(ingress.budget.used(), 0);
}

#[tokio::test]
async fn reload_keeps_old_bytes_charged_and_clamps_the_budget_to_one_body() {
    let ingress = ingress(16);
    let mut buffer = BudgetBuffer::new(Some(ingress.budget.clone()));
    buffer.push(b"abcdefghijkl").unwrap();
    let bytes = buffer.finish();
    ingress.reconfigure(
        IngressConfig {
            buffered_body_bytes: Some(1),
            ..IngressConfig::default()
        },
        8,
    );
    assert_eq!(ingress.budget.0.lock().unwrap().limit, Some(8));
    let mut other = BudgetBuffer::new(Some(ingress.budget.clone()));
    assert!(other.push(b"x").is_err());
    drop(bytes);
    other.push(b"x").unwrap();
    drop(other);
    assert_eq!(ingress.budget.used(), 0);
}

#[tokio::test]
async fn decoding_is_charged_in_addition_to_raw_storage() {
    use std::io::Write;
    let ingress = ingress(64);
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&[b'a'; 64]).unwrap();
    let compressed = encoder.finish().unwrap();
    let router = ingress.wrap(Router::new().route(
        "/",
        post(|request: Request| async move {
            let (parts, body) = request.into_parts();
            match crate::body::read_decoded(&parts.headers, body, 64).await {
                Ok(bytes) => Response::new(Body::from(bytes)),
                Err(response) => response,
            }
        }),
    ));
    let response = router
        .oneshot(
            Request::post("/")
                .header(header::CONTENT_ENCODING, "gzip")
                .body(Body::from(compressed))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(ingress.budget.used(), 0);
}

#[tokio::test]
async fn multipart_parser_copies_are_reserved_before_reading() {
    let ingress = ingress(6);
    let polled = Arc::new(AtomicUsize::new(0));
    let probe = polled.clone();
    let body = Body::from_stream(stream::once(async move {
        probe.fetch_add(1, Ordering::SeqCst);
        Ok::<_, std::io::Error>(Bytes::from_static(b"abcd"))
    }));
    let response = app(&ingress)
        .oneshot(
            Request::post("/")
                .header(header::CONTENT_LENGTH, "4")
                .header(header::CONTENT_TYPE, "multipart/form-data; boundary=x")
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(polled.load(Ordering::SeqCst), 0);
    assert_eq!(ingress.budget.used(), 0);
}

#[tokio::test]
async fn the_shared_body_reader_preserves_budget_ownership_without_recopying() {
    let ingress = ingress(8);
    let mut buffer = BudgetBuffer::new(Some(ingress.budget.clone()));
    buffer.push(b"abcd").unwrap();
    let bytes = crate::body::read_raw(&HeaderMap::new(), Body::from(buffer.finish()), 8)
        .await
        .unwrap();
    assert_eq!(ingress.budget.used(), 4);
    assert_eq!(bytes, "abcd");
    drop(bytes);
    assert_eq!(ingress.budget.used(), 0);
}

#[tokio::test]
async fn an_upgrade_header_cannot_bypass_upload_admission() {
    let ingress = ingress(4);
    let mut held = BudgetBuffer::new(Some(ingress.budget.clone()));
    held.push(b"held").unwrap();
    for method in [Method::GET, Method::POST] {
        let response = app(&ingress)
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/")
                    .header(header::UPGRADE, "websocket")
                    .header(header::CONTENT_LENGTH, "1")
                    .body(Body::from("x"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 503);
    }
    drop(held);
    assert_eq!(ingress.budget.used(), 0);
}

#[tokio::test(start_paused = true)]
async fn warnings_count_every_guard_hit_but_are_rate_limited() {
    let ingress = ingress(4);
    ingress.warn("test guard");
    ingress.warn("test guard");
    {
        let warnings = ingress.warnings.0.lock().unwrap();
        assert_eq!(warnings["test guard"].total, 2);
        assert_eq!(warnings["test guard"].since_log, 1);
    }
    tokio::time::advance(Duration::from_secs(31)).await;
    ingress.warn("test guard");
    let warnings = ingress.warnings.0.lock().unwrap();
    assert_eq!(warnings["test guard"].total, 3);
    assert_eq!(warnings["test guard"].since_log, 0);
}
