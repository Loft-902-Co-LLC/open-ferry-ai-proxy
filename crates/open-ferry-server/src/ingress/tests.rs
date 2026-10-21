use super::*;
use axum::routing::post;
use futures_util::stream;
use http_body_util::BodyExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;
use tower::ServiceExt;

fn ingress(limit: usize) -> Ingress {
    let ingress = Ingress::new(
        IngressConfig {
            buffered_body_bytes: Some(limit),
            body_idle_timeout: Some(Duration::from_secs(5)),
            body_total_timeout: Some(Duration::from_secs(20)),
            ..IngressConfig::default()
        },
        limit,
    );
    // Isolate exhaustion tests from the production multipart floor.
    ingress.budget.set_limit(Some(limit));
    ingress.budget.0.lock().unwrap().management_reserve = 0;
    ingress
}

fn app(ingress: &Ingress) -> Router {
    ingress.wrap(Router::new().route("/", post(|bytes: Bytes| async move { bytes })))
}

#[tokio::test]
async fn authentication_precedes_body_polling_and_budget_reservation() {
    use crate::config::ServerConfig;
    use crate::testing::{FakeCatalog, FakeDispatcher, state};
    let dispatcher = FakeDispatcher::new(vec![]);
    let state = state(
        ServerConfig {
            api_keys: vec!["client-secret".into()],
            body_limit: 4,
            ingress: IngressConfig {
                buffered_body_bytes: Some(12),
                ..IngressConfig::default()
            },
            ..ServerConfig::default()
        },
        FakeCatalog::new(),
        &dispatcher,
    );
    let router = crate::router(state.clone());
    let polls = Arc::new(AtomicUsize::new(0));
    for _ in 0..4 {
        let count = polls.clone();
        let response = router
            .clone()
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header(header::CONTENT_LENGTH, "67108864")
                    .body(Body::from_stream(stream::once(async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, std::io::Error>(Bytes::new())
                    })))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
        assert_eq!(state.ingress().reserved_body_bytes(), 0);
    }
    assert_eq!(polls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn authenticated_and_keyless_proxy_uploads_enforce_the_budget() {
    use crate::config::ServerConfig;
    use crate::testing::{FakeCatalog, FakeDispatcher, state};
    for keyed in [true, false] {
        let dispatcher = FakeDispatcher::new(vec![]);
        let state = state(
            ServerConfig {
                api_keys: if keyed {
                    vec!["client-secret".into()]
                } else {
                    vec![]
                },
                body_limit: 4,
                ingress: IngressConfig {
                    buffered_body_bytes: Some(12),
                    ..IngressConfig::default()
                },
                ..ServerConfig::default()
            },
            FakeCatalog::new(),
            &dispatcher,
        );
        let router = crate::router(state.clone());
        let mut tasks = Vec::new();
        for _ in 0..3 {
            let started = Arc::new(Notify::new());
            let mut request =
                Request::post("/v1/chat/completions").header(header::CONTENT_LENGTH, "4");
            if keyed {
                request = request.header(header::AUTHORIZATION, "Bearer client-secret");
            }
            tasks.push(tokio::spawn(
                router
                    .clone()
                    .oneshot(request.body(stalled(started.clone())).unwrap()),
            ));
            started.notified().await;
        }
        assert_eq!(state.ingress().reserved_body_bytes(), 12);
        let mut request = Request::post("/v1/chat/completions").header(header::CONTENT_LENGTH, "4");
        if keyed {
            request = request.header(header::AUTHORIZATION, "Bearer client-secret");
        }
        let response = router
            .oneshot(request.body(Body::from("abcd")).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 503);
        for task in tasks {
            task.abort();
            let _ = task.await;
        }
        assert_eq!(state.ingress().reserved_body_bytes(), 0);
    }
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
async fn reload_keeps_old_bytes_charged_and_clamps_the_budget_to_three_bodies() {
    let ingress = ingress(32);
    let mut proxy = ingress.budget.clone();
    proxy.1 = Pool::Proxy;
    let mut buffer = BudgetBuffer::new(Some(proxy.clone()));
    buffer.push(&[0; 28]).unwrap();
    let bytes = buffer.finish();
    ingress.reconfigure(
        IngressConfig {
            buffered_body_bytes: Some(1),
            ..IngressConfig::default()
        },
        8,
    );
    assert_eq!(
        ingress.budget.0.lock().unwrap().limit,
        Some(MANAGEMENT_RESERVE + 24)
    );
    let mut other = BudgetBuffer::new(Some(proxy));
    assert!(other.push(b"x").is_err());
    drop(bytes);
    other.push(b"x").unwrap();
    drop(other);
    assert_eq!(ingress.budget.used(), 0);
}

#[tokio::test]
async fn a_multipart_body_at_the_limit_fits_the_budget_floor() {
    let ingress = Ingress::new(
        IngressConfig {
            buffered_body_bytes: Some(16),
            ..IngressConfig::default()
        },
        16,
    );
    let response = app(&ingress)
        .oneshot(
            Request::post("/")
                .header(header::CONTENT_LENGTH, "16")
                .header(header::CONTENT_TYPE, "multipart/form-data; boundary=x")
                .body(Body::from("0123456789abcdef"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    drop(response);
    assert_eq!(ingress.budget.used(), 0);
    let raised = Ingress::new(IngressConfig::default(), 256 << 20);
    assert_eq!(
        raised.budget.0.lock().unwrap().limit,
        Some(MANAGEMENT_RESERVE + 3 * (256 << 20))
    );
}

#[test]
fn proxy_pressure_preserves_room_for_a_full_management_upload() {
    let ingress = Ingress::new(
        IngressConfig {
            buffered_body_bytes: Some(12),
            ..IngressConfig::default()
        },
        4,
    );
    let mut proxy = ingress.budget.clone();
    proxy.1 = Pool::Proxy;
    let mut held = Reservation {
        budget: Some(proxy.clone()),
        bytes: 0,
    };
    held.grow_to(12).unwrap();
    let mut extra = Reservation {
        budget: Some(proxy),
        bytes: 0,
    };
    assert!(extra.grow_to(1).is_err());
    let mut management = ingress.budget.clone();
    management.1 = Pool::Management;
    let mut upload = Reservation {
        budget: Some(management),
        bytes: 0,
    };
    upload.grow_to(MANAGEMENT_RESERVE).unwrap();
    assert_eq!(ingress.reserved_body_bytes(), 12 + MANAGEMENT_RESERVE);
    drop(upload);
    drop(held);
    assert_eq!(ingress.reserved_body_bytes(), 0);
}

#[tokio::test]
async fn an_upload_keeps_its_size_limit_across_a_reload() {
    use crate::config::ServerConfig;
    use crate::testing::{FakeCatalog, FakeDispatcher, state};
    let dispatcher = FakeDispatcher::new(vec![]);
    let state = state(
        ServerConfig {
            body_limit: 16,
            ..ServerConfig::default()
        },
        FakeCatalog::new(),
        &dispatcher,
    );
    let ingress = state.ingress().clone();
    let handler_state = state.clone();
    let router = ingress.wrap(Router::new().route(
        "/",
        post(move |request: Request| {
            let state = handler_state.clone();
            async move {
                let limit = body_limit(state.settings().config.body_limit);
                let (parts, body) = request.into_parts();
                match crate::body::read_raw(&parts.headers, body, limit).await {
                    Ok(bytes) => Response::new(Body::from(bytes)),
                    Err(response) => response,
                }
            }
        }),
    ));
    let started = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    let (signal, gate) = (started.clone(), proceed.clone());
    let body = Body::from_stream(stream::once(async move {
        signal.notify_one();
        gate.notified().await;
        Ok::<_, std::io::Error>(Bytes::from_static(b"abcdefghijkl"))
    }));
    let task = tokio::spawn(
        router.clone().oneshot(
            Request::post("/")
                .header(header::CONTENT_LENGTH, "12")
                .body(body)
                .unwrap(),
        ),
    );
    started.notified().await;
    state.set_config(ServerConfig {
        body_limit: 4,
        ..ServerConfig::default()
    });
    proceed.notify_one();
    let response = task.await.unwrap().unwrap();
    assert_eq!(response.status(), 200);
    drop(response);
    let response = router
        .oneshot(
            Request::post("/")
                .header(header::CONTENT_LENGTH, "12")
                .body(Body::from("abcdefghijkl"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
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
async fn zstd_working_storage_is_reserved_before_expansion() {
    let ingress = ingress(1 << 20);
    let router = ingress.wrap(Router::new().route(
        "/",
        post(|request: Request| async move {
            let (parts, body) = request.into_parts();
            match crate::body::read_decoded(&parts.headers, body, 1 << 20).await {
                Ok(bytes) => Response::new(Body::from(bytes)),
                Err(response) => response,
            }
        }),
    ));
    let compressed = ruzstd::encoding::compress_to_vec(
        &[b'a'; 4096][..],
        ruzstd::encoding::CompressionLevel::Fastest,
    );
    let raw_len = compressed.len();
    let response = router
        .oneshot(
            Request::post("/")
                .header(header::CONTENT_ENCODING, "zstd")
                .body(Body::from(compressed))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(ingress.budget.used(), 0);
    assert_eq!(ingress.budget.0.lock().unwrap().peak, raw_len);
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
async fn cancellation_after_admission_releases_regular_bodies_and_tracks_owned_upload_work() {
    for owned in [false, true] {
        let ingress = ingress(16);
        let started = Arc::new(Notify::new());
        let proceed = Arc::new(Notify::new());
        let (signal, gate) = (started.clone(), proceed.clone());
        let path = if owned { "/v0/management/upload" } else { "/" };
        let router = ingress.wrap(Router::new().route(
            path,
            post(move |bytes: Bytes| {
                let (signal, gate) = (signal.clone(), gate.clone());
                async move {
                    let copy = bytes.to_vec();
                    signal.notify_one();
                    gate.notified().await;
                    copy
                }
            }),
        ));
        let mut request = Request::post(path).header(header::CONTENT_LENGTH, "4");
        if owned {
            request = request.header(header::CONTENT_TYPE, "multipart/form-data; boundary=x");
        }
        let task = tokio::spawn(router.oneshot(request.body(Body::from("abcd")).unwrap()));
        started.notified().await;
        task.abort();
        let _ = task.await;
        if owned {
            assert_eq!(
                ingress.budget.used(),
                12,
                "a detached management upload lost its accounting"
            );
            proceed.notify_one();
            settle().await;
        }
        assert_eq!(ingress.budget.used(), 0);
    }
}

#[tokio::test]
async fn handler_storage_is_released_while_a_streamed_response_stays_open() {
    use std::io::Write;
    let ingress = Ingress::new(
        IngressConfig {
            buffered_body_bytes: Some(128),
            ..IngressConfig::default()
        },
        64,
    );
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&[b'a'; 64]).unwrap();
    let compressed = encoder.finish().unwrap();
    let router = ingress.wrap(Router::new().route(
        "/",
        post(|request: Request| async move {
            let (parts, body) = request.into_parts();
            let decoded = crate::body::read_decoded(&parts.headers, body, 64)
                .await
                .unwrap();
            let transformed = decoded.to_vec();
            drop(decoded);
            Response::new(Body::from_stream(
                stream::once(async move { Ok::<_, std::io::Error>(Bytes::from(transformed)) })
                    .chain(stream::pending()),
            ))
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
    assert_eq!(response.status(), 200);
    assert_eq!(ingress.budget.used(), 0);
    drop(response);
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
