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
    let config = IngressConfig { write_stall_timeout: Some(Duration::from_secs(2)), ..IngressConfig::default() };
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
async fn h2_headers_have_one_deadline_and_connect_data_is_not_a_header() {
    let config = IngressConfig::default();
    let traffic = Traffic::new(config.clone(), Ingress::new(config, 64), Instant::now());
    traffic.received(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    let flight = Flight::new(traffic.clone());
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
async fn h2_idle_deadline_excludes_open_streams_and_restarts_when_both_end() {
    let config = IngressConfig::default();
    let traffic = Traffic::new(config.clone(), Ingress::new(config, 64), Instant::now());
    traffic.received(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    let flight = Flight::new(traffic.clone());
    traffic.received(&[0, 0, 0, 1, 4, 0, 0, 0, 1]);
    drop(flight);
    tokio::time::advance(Duration::from_secs(200)).await;
    assert!(traffic.due().is_none());
    traffic.received(&[0, 0, 0, 0, 1, 0, 0, 0, 1]);
    assert!(
        traffic.due().is_none(),
        "one closed half is still an open stream"
    );
    traffic.written(&[0, 0, 0, 1, 5, 0, 0, 0, 1]);
    assert_eq!(
        traffic.due().unwrap().0,
        Instant::now() + Duration::from_secs(120)
    );
    tokio::time::advance(Duration::from_secs(121)).await;
    assert!(traffic.due().unwrap().0 <= Instant::now());
}
