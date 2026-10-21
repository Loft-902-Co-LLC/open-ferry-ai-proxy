use super::*;

#[test]
fn descriptor_reserve_two_listeners_and_two_handles_are_accounted_for() {
    assert_eq!(cap_from_limit(1024), 128);
    assert_eq!(cap_from_limit(8192), 1024);
    assert_eq!(cap_from_limit(64), 16);
    assert_eq!(cap_from_limit(256), 32);
    assert_eq!(cap_from_limit(u64::MAX), 8192);
}

#[test]
fn apple_descriptor_targets_are_clamped_and_small_limits_have_a_floor() {
    for (soft, hard, ceiling, target) in [
        (256, u64::MAX, 10240, 10240),
        (256, 10240, 10240, 10240),
        (1024, 524288, u64::MAX, 524288),
    ] {
        assert_eq!(raise_target(hard, ceiling), target);
        assert!(target >= soft);
        assert!(cap_from_limit(soft) >= (soft / 8).max(16) as usize);
    }
}

#[tokio::test(start_paused = true)]
async fn a_delayed_connection_task_keeps_the_accept_time_header_deadline() {
    let config = IngressConfig::default();
    let accepted_at = Instant::now();
    tokio::time::advance(Duration::from_secs(31)).await;
    let traffic = Traffic::new(config.clone(), Ingress::new(config, 64), accepted_at);
    assert_eq!(
        traffic.due().unwrap().0,
        accepted_at + Duration::from_secs(30)
    );
    assert!(traffic.due().unwrap().0 < Instant::now());
}

#[tokio::test(start_paused = true)]
async fn write_deadline_tracks_delivery_progress_and_not_upstream_silence() {
    let config = IngressConfig {
        write_stall_timeout: Some(Duration::from_secs(2)),
        ..IngressConfig::default()
    };
    let ingress = Ingress::new(config.clone(), 64);
    let traffic = Traffic::new(config, ingress, Instant::now());
    let flight = Flight::new(traffic.clone());
    tokio::time::advance(Duration::from_secs(200)).await;
    assert!(
        traffic.due().is_none(),
        "an active upstream wait was timed out"
    );
    let mut bytes = Delivery::new(Bytes::from_static(b"abc"), flight);
    tokio::time::advance(Duration::from_secs(1)).await;
    bytes.advance(1);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(traffic.due().unwrap().0 > Instant::now());
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(traffic.due().unwrap().0 <= Instant::now());
    bytes.advance(2);
    assert!(traffic.due().is_none());
}

#[tokio::test(start_paused = true)]
async fn a_queued_chunk_does_not_expire_while_its_stream_is_progressing() {
    let config = IngressConfig {
        write_stall_timeout: Some(Duration::from_secs(2)),
        ..IngressConfig::default()
    };
    let traffic = Traffic::new(config.clone(), Ingress::new(config, 64), Instant::now());
    traffic.received(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    let flight = Flight::new(traffic.clone());
    let mut first = Delivery::new(Bytes::from_static(b"abcdef"), flight.clone());
    let _second = Delivery::new(Bytes::from_static(b"queued"), flight);
    for _ in 0..5 {
        tokio::time::advance(Duration::from_secs(1)).await;
        first.advance(1);
        assert!(traffic.due().unwrap().0 > Instant::now());
    }
}

#[tokio::test(start_paused = true)]
async fn socket_progress_survives_successive_pending_tls_flushes() {
    struct StepSocket {
        writable: bool,
    }
    impl AsyncWrite for StepSocket {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            if std::mem::take(&mut self.writable) {
                Poll::Ready(Ok(bytes.len().min(1)))
            } else {
                Poll::Pending
            }
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    struct BufferedTls {
        socket: SocketIo<StepSocket>,
    }
    impl AsyncWrite for BufferedTls {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            if let Poll::Ready(Ok(_)) = Pin::new(&mut self.socket).poll_write(cx, b"encrypted") {
                assert!(
                    Pin::new(&mut self.socket)
                        .poll_write(cx, b"encrypted")
                        .is_pending()
                );
            }
            Poll::Pending
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    let config = IngressConfig {
        write_stall_timeout: Some(Duration::from_secs(2)),
        ..IngressConfig::default()
    };
    let traffic = Traffic::new(config.clone(), Ingress::new(config, 64), Instant::now());
    let _flight = Flight::new(traffic.clone());
    let mut io = Guarded {
        inner: BufferedTls {
            socket: SocketIo {
                inner: StepSocket { writable: false },
                traffic: traffic.clone(),
            },
        },
        traffic: traffic.clone(),
    };
    for _ in 0..5 {
        tokio::time::advance(Duration::from_secs(1)).await;
        io.inner.socket.inner.writable = true;
        std::future::poll_fn(|cx| {
            assert!(Pin::new(&mut io).poll_flush(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert!(traffic.due().unwrap().0 > Instant::now());
    }
    tokio::time::advance(Duration::from_secs(3)).await;
    assert!(traffic.due().unwrap().0 <= Instant::now());
}

#[tokio::test(start_paused = true)]
async fn h2_headers_have_one_deadline_and_connect_data_is_not_a_header() {
    let config = IngressConfig::default();
    let traffic = Traffic::new(config.clone(), Ingress::new(config, 64), Instant::now());
    traffic.received(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    let flight = Flight::new(traffic.clone());
    let _protocol = ProtocolTask::new(traffic.clone());
    traffic.received(&[0, 0, 3, 1, 0, 0, 0, 0, 1, 1]);
    let due = traffic.due().unwrap().0;
    tokio::time::advance(Duration::from_secs(10)).await;
    traffic.received(&[2, 3]);
    traffic.received(&[0, 0, 1, 9, 4, 0, 0, 0, 1]);
    assert_eq!(traffic.due().unwrap().0, due);
    traffic.received(&[4]);
    assert!(traffic.due().is_none());
    traffic.upgrade();
    drop(flight);
    traffic.received(&[0, 0, 5, 0, 0, 0, 0, 0, 1, 1]);
    tokio::time::advance(Duration::from_secs(200)).await;
    assert!(
        traffic.due().is_none(),
        "CONNECT DATA got a header/idle deadline"
    );
}

#[tokio::test(start_paused = true)]
async fn h2_idle_deadline_uses_accepted_protocol_task_lifetimes() {
    let config = IngressConfig::default();
    let traffic = Traffic::new(config.clone(), Ingress::new(config, 64), Instant::now());
    traffic.received(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    let flight = Flight::new(traffic.clone());
    let protocol = ProtocolTask::new(traffic.clone());
    drop(flight);
    tokio::time::advance(Duration::from_secs(200)).await;
    assert!(traffic.due().is_none());
    // Wire DATA, including late data on a reset stream, is never authoritative.
    traffic.received(&[0, 0, 0, 3, 0, 0, 0, 0, 7]);
    traffic.received(&[0, 0, 0, 0, 0, 0, 0, 0, 7]);
    assert!(traffic.due().is_none());
    drop(protocol);
    assert_eq!(
        traffic.due().unwrap().0,
        Instant::now() + Duration::from_secs(120)
    );
    traffic.received(&[0, 0, 0, 0, 0, 0, 0, 0, 7]);
    assert_eq!(traffic.state().protocol_tasks, 0);
    assert_eq!(
        traffic.due().unwrap().0,
        Instant::now() + Duration::from_secs(120)
    );
}

#[tokio::test(start_paused = true)]
async fn partial_headers_remain_armed_during_goaway() {
    let config = IngressConfig {
        header_timeout: Some(Duration::from_secs(2)),
        ..IngressConfig::default()
    };
    let traffic = Traffic::new(config.clone(), Ingress::new(config, 64), Instant::now());
    traffic.received(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    let flight = Flight::new(traffic.clone());
    flight.close_after_response();
    drop(flight);
    traffic.received(&[0, 0, 1, 1, 1, 0, 0, 0, 3, 0x82]);
    assert_eq!(traffic.state().protocol_tasks, 0);
    tokio::time::advance(Duration::from_secs(3)).await;
    assert_eq!(traffic.due().unwrap().1, "request headers");
    assert!(traffic.due().unwrap().0 <= Instant::now());
}

#[tokio::test(start_paused = true)]
async fn draining_has_an_absolute_bound_even_with_an_accepted_stream() {
    let config = IngressConfig {
        body_total_timeout: Some(Duration::from_secs(4)),
        ..IngressConfig::default()
    };
    let traffic = Traffic::new(config.clone(), Ingress::new(config, 64), Instant::now());
    traffic.received(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    let flight = Flight::new(traffic.clone());
    let _protocol = ProtocolTask::new(traffic.clone());
    flight.close_after_response();
    let started = Instant::now();
    for _ in 0..10 {
        tokio::time::advance(Duration::from_secs(1)).await;
        flight.close_after_response();
        traffic.received(&[0, 0, 1, 1, 4, 0, 0, 0, 3, 0x82]);
    }
    assert_eq!(traffic.due().unwrap().1, "drain deadline");
    assert_eq!(traffic.due().unwrap().0, started + Duration::from_secs(9));
}

#[tokio::test(start_paused = true)]
async fn a_reset_delivery_keeps_its_reclamation_deadline() {
    let config = IngressConfig {
        write_stall_timeout: Some(Duration::from_secs(2)),
        ..IngressConfig::default()
    };
    let traffic = Traffic::new(config.clone(), Ingress::new(config, 64), Instant::now());
    traffic.received(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    let flight = Flight::new(traffic.clone());
    let _queued = Delivery::new(Bytes::from_static(b"queued"), flight.clone());
    traffic
        .state()
        .deliveries
        .get_mut(&flight.id)
        .unwrap()
        .reset = true;
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(
        traffic
            .due()
            .is_some_and(|(at, _, stream)| at <= Instant::now() && stream.is_none()),
        "reset delivery lost its deadline"
    );
}
