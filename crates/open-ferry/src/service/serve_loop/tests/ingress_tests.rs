use super::*;
use axum::body::HttpBody;
use open_ferry_server::ingress::Ingress;

fn limits() -> IngressConfig {
    IngressConfig {
        max_connections: Some(4),
        header_timeout: Some(Duration::from_secs(2)),
        tls_handshake_timeout: Some(Duration::from_secs(2)),
        idle_timeout: Some(Duration::from_secs(3)),
        body_idle_timeout: Some(Duration::from_secs(2)),
        body_total_timeout: Some(Duration::from_secs(4)),
        write_stall_timeout: Some(Duration::from_secs(2)),
        ..IngressConfig::default()
    }
}

fn start_limited(
    config: IngressConfig,
    tls: Option<Arc<rustls::ServerConfig>>,
    routes: Option<Router>,
) -> Served {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let control = ServeControl::new();
    let gate = Gate::default();
    let ingress = Ingress::new(config, 1 << 20);
    let server = tokio::spawn(run(
        listener,
        tls,
        ingress.wrap(routes.unwrap_or_else(|| app(gate.clone()))),
        &control,
    ));
    Served {
        addr,
        control,
        gate,
        server,
    }
}

fn traffic(served: &Served) -> Option<Arc<Traffic>> {
    served
        .control
        .shared
        .connections()
        .map
        .values()
        .filter_map(Weak::upgrade)
        .find_map(|core| core.traffic.get().cloned())
}

async fn settle() {
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn ingress_header_deadline_closes_partial_heads_and_keepalive_gaps() {
    for keepalive in [false, true] {
        let served = start_limited(
            IngressConfig {
                idle_timeout: None,
                ..limits()
            },
            None,
            None,
        );
        let mut stream = connect(served.addr).await;
        if keepalive {
            get_request(&mut stream, "/", false).await;
            assert!(is_ok(&read_response(&mut stream).await));
        }
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: local")
            .await
            .unwrap();
        until(|| traffic(&served).is_some()).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(3)).await;
        settle().await;
        tokio::time::resume();
        assert_eq!(
            served.control.live_count(),
            0,
            "header deadline did not close the socket"
        );
        closed(&mut stream).await;
        served.control.stop();
        within(served.server).await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn ingress_tls_handshake_deadline_reclaims_admission() {
    let (tls, mut client, _dir) = certificates();
    client.alpn_protocols = vec![b"http/1.1".to_vec()];
    let served = start_limited(IngressConfig { max_connections: Some(1), ..limits() }, Some(tls), None);
    let mut stream = connect(served.addr).await;
    until(|| served.control.live_count() == 1).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(3)).await;
    settle().await;
    tokio::time::resume();
    assert_eq!(served.control.live_count(), 0);
    closed(&mut stream).await;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
    let mut next = tls_connect(&connector, connect(served.addr).await).await;
    get_request(&mut next, "/", true).await;
    assert!(is_ok(&read_response(&mut next).await));
    drop(next);
    served.control.stop();
    within(served.server).await.unwrap().unwrap();
}

#[tokio::test]
async fn ingress_cap_queues_in_backlog_and_recovers_without_half_registration() {
    let config = IngressConfig {
        max_connections: Some(2),
        header_timeout: None,
        idle_timeout: None,
        ..limits()
    };
    let served = start_limited(config, None, None);
    let first = connect(served.addr).await;
    let second = connect(served.addr).await;
    until(|| served.control.live_count() == 2).await;
    let mut queued = connect(served.addr).await;
    get_request(&mut queued, "/", true).await;
    let mut flood = Vec::new();
    for _ in 0..6 {
        flood.push(connect(served.addr).await);
    }
    assert!(stays_pending(read_response(&mut queued)).await);
    assert_eq!(served.control.live_count(), 2);
    assert_eq!(served.control.shared.connections().map.len(), 2);
    within(served.control.pause_accept()).await;
    within(served.control.resume_accept()).await.unwrap();
    drop(first);
    assert!(is_ok(&read_response(&mut queued).await));
    drop(second);
    drop(flood);
    drop(queued);
    until(|| served.control.live_count() == 0).await;
    let mut normal = connect(served.addr).await;
    get_request(&mut normal, "/", true).await;
    assert!(is_ok(&read_response(&mut normal).await));
    within(served.control.release()).await;
    within(served.server).await.unwrap().unwrap();
}

#[tokio::test]
async fn ingress_cap_is_per_listener_with_shared_controls() {
    let control = ServeControl::new();
    let ingress = Ingress::new(
        IngressConfig {
            max_connections: Some(1),
            header_timeout: None,
            idle_timeout: None,
            ..limits()
        },
        64,
    );
    let mut servers = Vec::new();
    let mut streams = Vec::new();
    for _ in 0..2 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        servers.push(tokio::spawn(run(
            listener,
            None,
            ingress.wrap(app(Gate::default())),
            &control,
        )));
        streams.push(connect(addr).await);
        streams.push(connect(addr).await);
    }
    until(|| control.live_count() == 2).await;
    assert!(stays_pending(control.wait_for(|stats| stats.live > 2)).await);
    control.cut();
    within(control.release()).await;
    for server in servers {
        within(server).await.unwrap().unwrap();
    }
}

async fn read_h2_frame(stream: &mut TcpStream) -> u8 {
    read_h2_packet(stream).await.0
}

async fn read_h2_packet(stream: &mut TcpStream) -> (u8, u8, u32, Vec<u8>) {
    let mut head = [0; 9];
    within(stream.read_exact(&mut head)).await.unwrap();
    let len = (usize::from(head[0]) << 16) | (usize::from(head[1]) << 8) | usize::from(head[2]);
    assert!(len < 1 << 20);
    let mut payload = vec![0; len];
    within(stream.read_exact(&mut payload)).await.unwrap();
    (head[3], head[4], u32::from_be_bytes(head[5..9].try_into().unwrap()) & 0x7fff_ffff, payload)
}

#[tokio::test]
async fn ingress_idle_http2_sends_goaway_and_partial_http2_heads_expire() {
    for partial in [false, true] {
        let served = start_limited(
            IngressConfig {
                header_timeout: partial.then_some(Duration::from_secs(2)),
                ..limits()
            },
            None,
            None,
        );
        let mut stream = connect(served.addr).await;
        stream
            .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\x00\x00\x00\x04\x00\x00\x00\x00\x00")
            .await
            .unwrap();
        assert_eq!(read_h2_frame(&mut stream).await, 4);
        if partial {
            stream
                .write_all(&[0, 0, 5, 1, 4, 0, 0, 0, 1, 0x82])
                .await
                .unwrap();
        }
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(4)).await;
        settle().await;
        tokio::time::resume();
        if partial {
            assert_eq!(served.control.live_count(), 0);
        } else {
            let mut goaway = false;
            for _ in 0..5 {
                if read_h2_frame(&mut stream).await == 7 {
                    goaway = true;
                    break;
                }
            }
            assert!(goaway, "idle HTTP/2 must send GOAWAY");
            // This peer deliberately never answers the graceful ping. It
            // still relinquishes its socket after the bounded GOAWAY grace.
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(6)).await;
            settle().await;
            tokio::time::resume();
            assert_eq!(served.control.live_count(), 0);
        }
        served.control.cut();
        within(served.server).await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn ingress_partial_headers_cannot_hold_a_draining_h2_connection() {
    let served = start_limited(limits(), None, None);
    let mut stream = raw_h2(&served, 65535).await;
    h2_get(&mut stream, 1, "/").await;
    loop { if read_h2_packet(&mut stream).await.0 == 0 { break; } }
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(4)).await;
    settle().await;
    tokio::time::resume();
    loop { if read_h2_frame(&mut stream).await == 7 { break; } }
    stream.write_all(&[0, 0, 1, 1, 1, 0, 0, 0, 3, 0x82]).await.unwrap();
    until(|| traffic(&served).is_some_and(|traffic| traffic.state().partial_head.is_some())).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(3)).await;
    settle().await;
    tokio::time::resume();
    assert_eq!(served.control.live_count(), 0);
    drop(stream);
    within(served.control.release()).await;
    within(served.server).await.unwrap().unwrap();
}

#[tokio::test]
async fn ingress_sse_upstream_silence_survives_every_idle_timer() {
    let (send, receive) = tokio::sync::mpsc::unbounded_channel::<axum::body::Bytes>();
    let receive = Arc::new(Mutex::new(Some(receive)));
    let routes = Router::new().route(
        "/sse",
        get(move || {
            let receive = receive.lock().unwrap().take().unwrap();
            async move {
                (
                    [("content-type", "text/event-stream")],
                    Body::from_stream(futures_util::stream::unfold(
                        receive,
                        |mut receive| async move {
                            receive
                                .recv()
                                .await
                                .map(|bytes| (Ok::<_, Infallible>(bytes), receive))
                        },
                    )),
                )
            }
        }),
    );
    let served = start_limited(limits(), None, Some(routes));
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(TokioIo::new(connect(served.addr).await))
            .await
            .unwrap();
    let driver = tokio::spawn(connection);
    send.send(axum::body::Bytes::from_static(b"data: first\n\n"))
        .unwrap();
    let mut response = within(
        sender.send_request(
            hyper::Request::get("/sse")
                .header("host", "localhost")
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .unwrap();
    let first = std::future::poll_fn(|cx| Pin::new(response.body_mut()).poll_frame(cx))
        .await
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
    assert_eq!(first, "data: first\n\n");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(200)).await;
    settle().await;
    tokio::time::resume();
    assert_eq!(served.control.live_count(), 1);
    send.send(axum::body::Bytes::from_static(b"data: second\n\n"))
        .unwrap();
    let second = within(std::future::poll_fn(|cx| {
        Pin::new(response.body_mut()).poll_frame(cx)
    }))
    .await
    .unwrap()
    .unwrap()
    .into_data()
    .unwrap();
    assert_eq!(second, "data: second\n\n");
    served.control.cut();
    within(served.server).await.unwrap().unwrap();
    driver.abort();
}

#[tokio::test]
async fn ingress_websockets_survive_all_upload_and_idle_timers_for_both_protocols() {
    for h2 in [false, true] {
        let served = start_limited(limits(), None, None);
        if h2 {
            let (mut sender, driver) = h2_client(connect(served.addr).await).await;
            let request = hyper::Request::builder()
                .method(hyper::Method::CONNECT)
                .uri(format!("http://{}/ws", served.addr))
                .header("sec-websocket-version", "13")
                .extension(hyper::ext::Protocol::from_static("websocket"))
                .body(Body::empty())
                .unwrap();
            let mut response = within(sender.send_request(request)).await.unwrap();
            assert_eq!(response.status(), 200);
            let upgraded = within(hyper::upgrade::on(&mut response)).await.unwrap();
            let mut socket =
                WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Client, None).await;
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(200)).await;
            settle().await;
            tokio::time::resume();
            socket
                .send(tokio_tungstenite::tungstenite::Message::text("still here"))
                .await
                .unwrap();
            assert_eq!(
                within(socket.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .into_text()
                    .unwrap(),
                "still here"
            );
            assert_eq!(served.control.cut(), 1);
            driver.abort();
        } else {
            let (mut socket, _) = within(tokio_tungstenite::connect_async(format!(
                "ws://{}/ws",
                served.addr
            )))
            .await
            .unwrap();
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(200)).await;
            settle().await;
            tokio::time::resume();
            socket
                .send(tokio_tungstenite::tungstenite::Message::text("still here"))
                .await
                .unwrap();
            assert_eq!(
                within(socket.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .into_text()
                    .unwrap(),
                "still here"
            );
            assert_eq!(served.control.cut(), 1);
        }
        within(served.control.release()).await;
        within(served.server).await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn ingress_slow_sse_reader_is_cut_and_live_handles_return_to_baseline() {
    let routes = Router::new().route(
        "/sse",
        get(|| async {
            (
                [("content-type", "text/event-stream")],
                Body::from_stream(futures_util::stream::repeat_with(|| {
                    Ok::<_, Infallible>(axum::body::Bytes::from_static(&[b'x'; 64 * 1024]))
                })),
            )
        }),
    );
    let served = start_limited(limits(), None, Some(routes));
    let mut stream = connect(served.addr).await;
    SockRef::from(&stream).set_recv_buffer_size(1024).unwrap();
    until(|| traffic(&served).is_some()).await;
    let io_owner = {
        let core = served
            .control
            .shared
            .connections()
            .map
            .values()
            .find_map(Weak::upgrade)
            .unwrap();
        core.socket
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .set_send_buffer_size(4096)
            .unwrap();
        Arc::downgrade(&core)
    };
    get_request(&mut stream, "/sse", false).await;
    until(|| traffic(&served).is_some_and(|traffic| traffic.has_pending_write())).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(3)).await;
    settle().await;
    tokio::time::resume();
    assert_eq!(
        served.control.live_count(),
        0,
        "write stall did not reclaim live handles"
    );
    within(served.control.release()).await;
    within(served.server).await.unwrap().unwrap();
    drop(stream);
    assert!(
        io_owner.upgrade().is_none(),
        "the socket owner retained an OS handle"
    );
}

#[tokio::test]
async fn ingress_release_and_cut_keep_their_behavior_with_guards_active() {
    let served = start_limited(limits(), None, None);
    let mut stream = connect(served.addr).await;
    get_request(&mut stream, "/slow", false).await;
    within(served.gate.started.notified()).await;
    let drained = served.control.release();
    assert_eq!(served.control.live_count(), 1);
    served.gate.proceed.notify_one();
    let response = read_response(&mut stream).await;
    assert!(is_ok(&response));
    assert!(response.0.contains("connection: close"));
    within(drained).await;
    within(served.server).await.unwrap().unwrap();
    let served = start_limited(limits(), None, None);
    let mut stream = connect(served.addr).await;
    get_request(&mut stream, "/slow", false).await;
    within(served.gate.started.notified()).await;
    let drained = served.control.release();
    assert_eq!(served.control.cut(), 1);
    within(drained).await;
    closed(&mut stream).await;
    within(served.server).await.unwrap().unwrap();
}

#[tokio::test]
async fn ingress_body_timeout_answers_408_then_closes_the_socket() {
    let served = start_limited(limits(), None, None);
    let mut stream = connect(served.addr).await;
    stream
        .write_all(b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 16\r\n\r\nab")
        .await
        .unwrap();
    until(|| {
        served
            .control
            .shared
            .connections()
            .map
            .values()
            .filter_map(Weak::upgrade)
            .any(|core| core.has_seen_request())
    })
    .await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(3)).await;
    settle().await;
    tokio::time::resume();
    let response = read_response(&mut stream).await;
    assert!(response.0.starts_with("http/1.1 408"));
    assert!(response.0.contains("connection: close"));
    closed(&mut stream).await;
    until(|| served.control.live_count() == 0).await;
    within(served.control.release()).await;
    within(served.server).await.unwrap().unwrap();
}

async fn raw_h2(served: &Served, window: u32) -> TcpStream {
    let mut stream = connect(served.addr).await;
    stream.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n").await.unwrap();
    let mut settings = vec![0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 4];
    settings.extend_from_slice(&window.to_be_bytes());
    stream.write_all(&settings).await.unwrap();
    stream
}

async fn h2_get(stream: &mut TcpStream, id: u32, path: &str) {
    let mut block = vec![0x82, 0x86, 4, path.len() as u8];
    block.extend_from_slice(path.as_bytes());
    block.extend_from_slice(b"\x01\x09localhost");
    let mut frame = vec![0, 0, block.len() as u8, 1, 5];
    frame.extend_from_slice(&id.to_be_bytes());
    frame.extend_from_slice(&block);
    stream.write_all(&frame).await.unwrap();
}

async fn h2_window(stream: &mut TcpStream, id: u32, amount: u32) {
    let mut frame = vec![0, 0, 4, 8, 0];
    frame.extend_from_slice(&id.to_be_bytes());
    frame.extend_from_slice(&amount.to_be_bytes());
    stream.write_all(&frame).await.unwrap();
}

fn sse_routes() -> Router {
    Router::new().route("/sse", get(|| async {
        ([ ("content-type", "text/event-stream") ],
        Body::from_stream(futures_util::stream::repeat_with(|| {
            Ok::<_, Infallible>(axum::body::Bytes::from_static(&[b'x'; 64 * 1024]))
        })))
    }))
}

#[tokio::test]
async fn ingress_h2_resets_only_the_stalled_stream_and_keeps_its_peer_stream() {
    let gate = Gate::default();
    let route_gate = gate.clone();
    let routes = sse_routes().route("/ok", get(move || {
        let gate = route_gate.clone();
        async move { gate.started.notify_one(); gate.proceed.notified().await; "ok" }
    }));
    let served = start_limited(limits(), None, Some(routes));
    let mut stream = raw_h2(&served, 1).await;
    h2_get(&mut stream, 1, "/sse").await;
    loop { let frame = read_h2_packet(&mut stream).await; if frame.0 == 0 && frame.2 == 1 { break; } }
    h2_get(&mut stream, 3, "/ok").await;
    within(gate.started.notified()).await;
    assert!(traffic(&served).unwrap().has_pending_delivery());
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(3)).await;
    settle().await;
    tokio::time::resume();
    assert_eq!(served.control.live_count(), 1);
    loop {
        let frame = read_h2_packet(&mut stream).await;
        if frame.0 == 3 && frame.2 == 1 {
            assert_eq!(u32::from_be_bytes(frame.3.try_into().unwrap()), 8, "expected CANCEL");
            break;
        }
    }
    gate.proceed.notify_one();
    h2_window(&mut stream, 3, 32).await;
    loop {
        let frame = read_h2_packet(&mut stream).await;
        if frame.0 == 0 && frame.2 == 3 { assert_eq!(frame.3, b"ok"); assert_ne!(frame.1 & 1, 0); break; }
    }
    assert_eq!(served.control.live_count(), 1);
    drop(stream);
    until(|| served.control.live_count() == 0).await;
    within(served.control.release()).await;
    within(served.server).await.unwrap().unwrap();
}

#[tokio::test]
async fn ingress_h2_slow_readers_can_progress_for_longer_than_the_deadline() {
    let served = start_limited(limits(), None, Some(sse_routes()));
    let mut stream = raw_h2(&served, 1).await;
    h2_get(&mut stream, 1, "/sse").await;
    for _ in 0..5 {
        loop {
            let frame = read_h2_packet(&mut stream).await;
            assert_ne!(frame.0, 3, "progressing stream was reset");
            if frame.0 == 0 && frame.2 == 1 { assert_eq!(frame.3.len(), 1); break; }
        }
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        tokio::time::resume();
        h2_window(&mut stream, 1, 1).await;
    }
    assert_eq!(served.control.live_count(), 1);
    drop(stream);
    until(|| served.control.live_count() == 0).await;
    within(served.control.release()).await;
    within(served.server).await.unwrap().unwrap();
}

#[tokio::test]
async fn ingress_h2_connection_window_stall_still_closes_the_connection() {
    let served = start_limited(limits(), None, Some(sse_routes()));
    let mut stream = raw_h2(&served, 1 << 20).await;
    h2_get(&mut stream, 1, "/sse").await;
    let mut received = 0;
    while received < 65535 {
        let frame = read_h2_packet(&mut stream).await;
        if frame.0 == 0 { received += frame.3.len(); }
    }
    assert_eq!(received, 65535);
    assert_eq!(traffic(&served).unwrap().state().send_window, 0);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(3)).await;
    settle().await;
    tokio::time::resume();
    assert_eq!(served.control.live_count(), 0);
    drop(stream);
    within(served.control.release()).await;
    within(served.server).await.unwrap().unwrap();
}

#[tokio::test]
async fn ingress_h2_oversize_rejection_does_not_drain_the_connection() {
    let served = start_limited(IngressConfig { header_timeout: None, idle_timeout: None, ..limits() }, None, None);
    let mut stream = raw_h2(&served, 65535).await;
    let block = b"\x82\x86\x84\x01\x09localhost\x00\x0econtent-length\x071048577";
    let mut head = vec![0, 0, block.len() as u8, 1, 4, 0, 0, 0, 1];
    head.extend_from_slice(block);
    stream.write_all(&head).await.unwrap();
    loop {
        let frame = read_h2_packet(&mut stream).await;
        assert_ne!(frame.0, 7, "a size rejection sent GOAWAY");
        if frame.0 == 0 && frame.2 == 1 { break; }
    }
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(6)).await;
    settle().await;
    tokio::time::resume();
    assert_eq!(served.control.live_count(), 1);
    h2_get(&mut stream, 3, "/").await;
    loop {
        let frame = read_h2_packet(&mut stream).await;
        assert_ne!(frame.0, 7);
        if frame.0 == 0 && frame.2 == 3 { assert_eq!(frame.3, b"ok"); break; }
    }
    drop(stream);
    until(|| served.control.live_count() == 0).await;
    within(served.control.release()).await;
    within(served.server).await.unwrap().unwrap();
}

#[tokio::test]
async fn ingress_capacity_reload_wakes_a_full_listener() {
    let config = IngressConfig {
        max_connections: Some(1),
        header_timeout: None,
        idle_timeout: None,
        ..limits()
    };
    let ingress = Ingress::new(config.clone(), 64);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let control = ServeControl::new();
    let server = tokio::spawn(run(
        listener,
        None,
        ingress.wrap(app(Gate::default())),
        &control,
    ));
    let first = connect(address).await;
    until(|| control.live_count() == 1).await;
    let mut queued = connect(address).await;
    get_request(&mut queued, "/", true).await;
    assert!(stays_pending(read_response(&mut queued)).await);
    ingress.reconfigure(
        IngressConfig {
            max_connections: Some(2),
            ..config
        },
        64,
    );
    assert!(is_ok(&read_response(&mut queued).await));
    drop(first);
    within(control.release()).await;
    within(server).await.unwrap().unwrap();
}
