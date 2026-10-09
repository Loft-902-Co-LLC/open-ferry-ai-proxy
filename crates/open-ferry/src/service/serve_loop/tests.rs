use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::extract::ConnectInfo;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::{any, get};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::Role;

use super::*;

/// How long a test waits for something that should happen.
const WAIT: Duration = Duration::from_secs(10);

/// How long a test waits to see that something doesn't happen.
const QUIET: Duration = Duration::from_millis(300);

/// What a handler that holds a request open signals and waits for.
#[derive(Clone, Default)]
struct Gate {
    started: Arc<Notify>,
    proceed: Arc<Notify>,
}

fn app(gate: Gate) -> Router {
    Router::new()
        .route(
            "/",
            get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move {
                format!("hello {}", peer.ip())
            }),
        )
        .route(
            "/slow",
            get(move || async move {
                gate.started.notify_one();
                gate.proceed.notified().await;
                "done"
            }),
        )
        .route("/ws", any(echo))
        .route("/hold", any(hold))
}

/// Takes the upgraded socket and never touches it again, as a handler
/// waiting on something else does.
async fn hold(upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(|socket| async move {
        let _socket = socket;
        std::future::pending::<()>().await;
    })
}

async fn echo(upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(|mut socket| async move {
        while let Some(Ok(message)) = socket.recv().await {
            if matches!(message, Message::Close(_)) || socket.send(message).await.is_err() {
                break;
            }
        }
    })
}

struct Served {
    addr: SocketAddr,
    control: ServeControl,
    gate: Gate,
    server: JoinHandle<io::Result<()>>,
}

/// A loop on a loopback port serving [`app`].
fn start(tls: Option<Arc<rustls::ServerConfig>>, deadline: Duration) -> Served {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let control = ServeControl::with_first_request_deadline(deadline);
    let gate = Gate::default();
    let server = tokio::spawn(run(listener, tls, app(gate.clone()), &control));
    Served {
        addr,
        control,
        gate,
        server,
    }
}

async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, future).await.expect("timed out")
}

/// Whether `future` is still pending after [`QUIET`].
async fn stays_pending<T>(future: impl Future<Output = T>) -> bool {
    tokio::time::timeout(QUIET, future).await.is_err()
}

/// Waits for `condition` to hold, and fails if it doesn't within [`WAIT`].
async fn until(mut condition: impl FnMut() -> bool) {
    within(async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
}

async fn connect(addr: SocketAddr) -> TcpStream {
    within(TcpStream::connect(addr)).await.unwrap()
}

async fn get_request<S: AsyncWriteExt + Unpin>(stream: &mut S, path: &str, close: bool) {
    let connection = if close { "Connection: close\r\n" } else { "" };
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n{connection}\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
}

/// One HTTP/1.1 response: its head and its body, lowercased in the head.
async fn read_response<S: AsyncRead + Unpin>(stream: &mut S) -> (String, String) {
    within(async {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 1024];
        let end = loop {
            if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(
                read > 0,
                "closed in a response: {:?}",
                String::from_utf8_lossy(&bytes)
            );
            bytes.extend_from_slice(&chunk[..read]);
        };
        let head = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .map_or(0, |length| length.trim().parse::<usize>().unwrap());
        while bytes.len() < end + length {
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "closed in a body");
            bytes.extend_from_slice(&chunk[..read]);
        }
        let body = String::from_utf8_lossy(&bytes[end..end + length]).into_owned();
        (head, body)
    })
    .await
}

/// Waits for the server to close `stream`.
async fn closed<S: AsyncRead + Unpin>(stream: &mut S) {
    within(async {
        let mut chunk = [0u8; 256];
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
    .await;
}

fn is_ok(response: &(String, String)) -> bool {
    response.0.starts_with("http/1.1 200")
}

fn certificates() -> (
    Arc<rustls::ServerConfig>,
    rustls::ClientConfig,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert, generated.cert.pem()).unwrap();
    std::fs::write(&key, generated.signing_key.serialize_pem()).unwrap();
    let server = crate::tls::load(cert.to_str().unwrap(), key.to_str().unwrap()).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(generated.cert.der().clone()).unwrap();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    client.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    (server, client, dir)
}

async fn tls_connect(
    connector: &tokio_rustls::TlsConnector,
    stream: TcpStream,
) -> tokio_rustls::client::TlsStream<TcpStream> {
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    within(connector.connect(name, stream)).await.unwrap()
}

/// An HTTP/2 connection with prior knowledge: its sender and its driver.
async fn h2_client<I>(
    io: I,
) -> (
    hyper::client::conn::http2::SendRequest<Body>,
    JoinHandle<hyper::Result<()>>,
)
where
    I: AsyncRead + AsyncWriteExt + Unpin + Send + 'static,
{
    let (sender, connection) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(io))
            .await
            .unwrap();
    (sender, tokio::spawn(connection))
}

fn h2_get(path: &str) -> hyper::Request<Body> {
    hyper::Request::get(format!("http://localhost{path}"))
        .body(Body::empty())
        .unwrap()
}

async fn body_text(response: hyper::Response<hyper::body::Incoming>) -> String {
    let bytes = axum::body::to_bytes(Body::new(response.into_body()), usize::MAX)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

// Not upstream's: the first-request deadline is the 10 s the design gives.
#[test]
fn first_request_deadline_is_ten_seconds() {
    assert_eq!(FIRST_REQUEST_DEADLINE, Duration::from_secs(10));
}

// Not upstream's: the loop serves, hands handlers the peer's address as
// `axum::serve` does, and ends once it is stopped.
#[tokio::test]
async fn serves_with_the_peers_address_and_stops() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
    let mut stream = connect(served.addr).await;
    get_request(&mut stream, "/", false).await;
    let response = read_response(&mut stream).await;
    assert!(is_ok(&response), "{response:?}");
    assert_eq!(response.1, "hello 127.0.0.1");
    assert_eq!(served.control.live_count(), 1);

    served.control.stop();
    closed(&mut stream).await;
    within(served.server).await.unwrap().unwrap();
    assert_eq!(served.control.live_count(), 0);
}

// Not upstream's: a pause drops the listener and keeps the socket, so a
// client that connects meanwhile waits in the backlog and is served after
// the resume, however often the loop pauses and resumes.
#[tokio::test]
async fn pause_and_resume_keep_the_backlog() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
    let mut stream = connect(served.addr).await;
    get_request(&mut stream, "/", true).await;
    assert!(is_ok(&read_response(&mut stream).await));

    for _ in 0..3 {
        within(served.control.pause_accept()).await;
        assert_eq!(served.control.stats().accepting, 0);
        let mut waiting = connect(served.addr).await;
        get_request(&mut waiting, "/", true).await;
        assert!(
            stays_pending(read_response(&mut waiting)).await,
            "served while paused"
        );
        within(served.control.resume_accept()).await.unwrap();
        assert_eq!(served.control.stats().accepting, 1);
        let response = read_response(&mut waiting).await;
        assert!(is_ok(&response), "{response:?}");
    }
    served.control.stop();
    within(served.server).await.unwrap().unwrap();
}

// Not upstream's: connections already accepted are served while the loop
// is paused.
#[tokio::test]
async fn a_pause_leaves_open_connections_alone() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
    let mut stream = connect(served.addr).await;
    get_request(&mut stream, "/", false).await;
    assert!(is_ok(&read_response(&mut stream).await));
    within(served.control.pause_accept()).await;
    get_request(&mut stream, "/", false).await;
    assert!(is_ok(&read_response(&mut stream).await));
    within(served.control.resume_accept()).await.unwrap();
    served.control.stop();
    within(served.server).await.unwrap().unwrap();
}

// Not upstream's: a release closes an idle keep-alive HTTP/1 connection,
// stops the loop and completes once the connection is gone.
#[tokio::test]
async fn release_drains_a_keep_alive_connection() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
    let mut stream = connect(served.addr).await;
    get_request(&mut stream, "/", false).await;
    let response = read_response(&mut stream).await;
    assert!(is_ok(&response) && !response.0.contains("connection: close"));

    let drained = served.control.release();
    closed(&mut stream).await;
    within(drained).await;
    assert_eq!(served.control.live_count(), 0);
    within(served.server).await.unwrap().unwrap();
}

// Not upstream's: a request in flight at the release is answered, with
// `Connection: close`, and the connection then closes.
#[tokio::test]
async fn release_lets_a_request_in_flight_finish_with_connection_close() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
    let mut stream = connect(served.addr).await;
    get_request(&mut stream, "/slow", false).await;
    within(served.gate.started.notified()).await;

    let drained = served.control.release();
    tokio::pin!(drained);
    assert!(stays_pending(&mut drained).await);
    served.gate.proceed.notify_one();
    let response = read_response(&mut stream).await;
    assert!(is_ok(&response), "{response:?}");
    assert!(response.0.contains("connection: close"), "{response:?}");
    assert_eq!(response.1, "done");
    closed(&mut stream).await;
    within(drained).await;
}

// Not upstream's: a release sends GOAWAY on an HTTP/2 connection, lets the
// stream in flight finish and ends the connection, which takes no more.
#[tokio::test]
async fn release_sends_goaway_on_http2() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
    let (mut sender, driver) = h2_client(connect(served.addr).await).await;
    let response = within(sender.send_request(h2_get("/"))).await.unwrap();
    assert_eq!(body_text(response).await, "hello 127.0.0.1");
    let pending = tokio::spawn(sender.send_request(h2_get("/slow")));
    within(served.gate.started.notified()).await;

    let drained = served.control.release();
    // Past the first GOAWAY, which still takes streams until its second.
    tokio::time::sleep(QUIET).await;
    served.gate.proceed.notify_one();
    let response = within(pending).await.unwrap().unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(body_text(response).await, "done");
    within(driver).await.unwrap().unwrap();
    assert!(within(sender.ready()).await.is_err());
    within(drained).await;
    assert_eq!(served.control.live_count(), 0);
}

// Not upstream's: a WebSocket over HTTP/1 outlives a release, counted as
// upgraded, until it closes; then the release completes.
#[tokio::test]
async fn a_websocket_over_http1_survives_a_release_until_it_closes() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
    let (mut socket, _) = within(tokio_tungstenite::connect_async(format!(
        "ws://{}/ws",
        served.addr
    )))
    .await
    .unwrap();
    socket
        .send(tokio_tungstenite::tungstenite::Message::text("one"))
        .await
        .unwrap();
    assert_eq!(
        within(socket.next())
            .await
            .unwrap()
            .unwrap()
            .to_text()
            .unwrap(),
        "one"
    );
    assert_eq!(served.control.upgraded_count(), 1);

    let drained = served.control.release();
    tokio::pin!(drained);
    assert!(stays_pending(&mut drained).await);
    assert_eq!(served.control.live_count(), 1);
    socket
        .send(tokio_tungstenite::tungstenite::Message::text("two"))
        .await
        .unwrap();
    assert_eq!(
        within(socket.next())
            .await
            .unwrap()
            .unwrap()
            .to_text()
            .unwrap(),
        "two"
    );

    within(socket.close(None)).await.unwrap();
    within(drained).await;
    assert_eq!(served.control.live_count(), 0);
    assert_eq!(served.control.upgraded_count(), 0);
}

// Not upstream's: the same over HTTP/2, by extended CONNECT, which the
// loop has enabled as `axum::serve` does.
#[tokio::test]
async fn a_websocket_over_http2_survives_a_release_until_it_closes() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
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
    socket
        .send(tokio_tungstenite::tungstenite::Message::text("one"))
        .await
        .unwrap();
    assert_eq!(
        within(socket.next())
            .await
            .unwrap()
            .unwrap()
            .to_text()
            .unwrap(),
        "one"
    );

    let drained = served.control.release();
    tokio::pin!(drained);
    assert!(stays_pending(&mut drained).await);
    assert_eq!(served.control.live_count(), 1);
    socket
        .send(tokio_tungstenite::tungstenite::Message::text("two"))
        .await
        .unwrap();
    assert_eq!(
        within(socket.next())
            .await
            .unwrap()
            .unwrap()
            .to_text()
            .unwrap(),
        "two"
    );

    within(socket.close(None)).await.unwrap();
    drop(socket);
    drop(sender);
    let _ = within(driver).await;
    within(drained).await;
    assert_eq!(served.control.live_count(), 0);
}

// Not upstream's: a stop doesn't wait for an upgraded connection, as
// `axum::serve`'s graceful shutdown doesn't, and the connection works on.
#[tokio::test]
async fn a_stop_does_not_wait_for_a_websocket() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
    let (mut socket, _) = within(tokio_tungstenite::connect_async(format!(
        "ws://{}/ws",
        served.addr
    )))
    .await
    .unwrap();
    socket
        .send(tokio_tungstenite::tungstenite::Message::text("one"))
        .await
        .unwrap();
    assert_eq!(
        within(socket.next())
            .await
            .unwrap()
            .unwrap()
            .to_text()
            .unwrap(),
        "one"
    );

    served.control.stop();
    within(served.server).await.unwrap().unwrap();
    assert_eq!(served.control.live_count(), 1);
    assert_eq!(served.control.cut(), 1);
    assert!(matches!(within(socket.next()).await, None | Some(Err(_))));
}

// Not upstream's: a TLS handshake under way at the release is counted, so
// the release waits for it; the request that follows is answered with
// `Connection: close`, over HTTP/1 and over HTTP/2.
#[tokio::test]
async fn a_tls_handshake_in_progress_at_release_gets_its_request_answered() {
    let (config, client, _dir) = certificates();
    for http2 in [false, true] {
        let served = start(Some(config.clone()), FIRST_REQUEST_DEADLINE);
        let stalled = connect(served.addr).await;
        until(|| served.control.live_count() > 0).await;

        let drained = served.control.release();
        tokio::pin!(drained);
        assert!(stays_pending(&mut drained).await);
        assert_eq!(served.control.live_count(), 1);

        let mut client = client.clone();
        if !http2 {
            client.alpn_protocols = vec![b"http/1.1".to_vec()];
        }
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
        let stream = tls_connect(&connector, stalled).await;
        if http2 {
            let (mut sender, driver) = h2_client(stream).await;
            let response = within(sender.send_request(h2_get("/"))).await.unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(body_text(response).await, "hello 127.0.0.1");
            let _ = within(driver).await;
        } else {
            let mut stream = stream;
            get_request(&mut stream, "/", false).await;
            let response = read_response(&mut stream).await;
            assert!(is_ok(&response), "{response:?}");
            assert!(response.0.contains("connection: close"), "{response:?}");
            assert_eq!(response.1, "hello 127.0.0.1");
            closed(&mut stream).await;
        }
        within(drained).await;
        within(served.server).await.unwrap().unwrap();
    }
}

// Not upstream's: a cut ends a TLS handshake that is under way.
#[tokio::test]
async fn a_cut_ends_a_tls_handshake() {
    let (config, _client, _dir) = certificates();
    let served = start(Some(config), FIRST_REQUEST_DEADLINE);
    let mut stalled = connect(served.addr).await;
    until(|| served.control.live_count() > 0).await;
    let drained = served.control.release();
    assert_eq!(served.control.cut(), 1);
    closed(&mut stalled).await;
    assert_eq!(served.control.live_count(), 0);
    within(drained).await;
    assert_eq!(served.control.cut(), 0);
}

// Not upstream's: a connection that has carried no request is left alone
// until the release, then given the first-request deadline, which closes
// it; one that sends its request within the deadline is answered.
#[tokio::test]
async fn the_first_request_deadline_closes_a_connection_that_sends_none() {
    let served = start(None, Duration::from_millis(500));
    let mut silent = connect(served.addr).await;
    let mut late = connect(served.addr).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(served.control.live_count(), 2, "closed before the release");

    let released = Instant::now();
    let drained = served.control.release();
    tokio::time::sleep(Duration::from_millis(100)).await;
    get_request(&mut late, "/", false).await;
    let response = read_response(&mut late).await;
    assert!(is_ok(&response), "{response:?}");
    assert!(response.0.contains("connection: close"), "{response:?}");
    closed(&mut late).await;

    closed(&mut silent).await;
    assert!(
        released.elapsed() >= Duration::from_millis(450),
        "closed after {:?}",
        released.elapsed()
    );
    within(drained).await;
    within(served.server).await.unwrap().unwrap();
}

// Not upstream's: the deadline holds for HTTP/2 too: a client that made the
// connection and sends no stream is closed.
#[tokio::test]
async fn the_first_request_deadline_closes_an_http2_connection_that_sends_none() {
    let served = start(None, Duration::from_millis(300));
    let (sender, driver) = h2_client(connect(served.addr).await).await;
    // A connection the loop hasn't accepted is reset when the release drops
    // the only handle on the socket.
    until(|| served.control.live_count() > 0).await;
    let drained = served.control.release();
    let released = Instant::now();
    let _ = within(driver).await;
    assert!(released.elapsed() >= Duration::from_millis(250));
    drop(sender);
    within(drained).await;
}

// Not upstream's: a cut closes what is left of a release, counts each
// connection once, and fails the IO of an upgraded one.
#[tokio::test]
async fn cut_closes_what_is_left_and_counts_it() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
    let mut silent = connect(served.addr).await;
    let mut in_flight = connect(served.addr).await;
    get_request(&mut in_flight, "/slow", false).await;
    within(served.gate.started.notified()).await;
    let (mut socket, _) = within(tokio_tungstenite::connect_async(format!(
        "ws://{}/ws",
        served.addr
    )))
    .await
    .unwrap();

    let drained = served.control.release();
    tokio::pin!(drained);
    assert!(stays_pending(&mut drained).await);
    assert_eq!(served.control.live_count(), 3);

    assert_eq!(served.control.cut(), 3);
    assert_eq!(served.control.cut(), 0);
    closed(&mut silent).await;
    closed(&mut in_flight).await;
    assert!(matches!(within(socket.next()).await, None | Some(Err(_))));
    within(drained).await;
    assert_eq!(served.control.live_count(), 0);
}

// Not upstream's: dropping the future that serves, as a shutdown that gave
// up does, closes its connections when the service's wrapper asks.
#[tokio::test]
async fn cut_on_drop_closes_the_connections() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
    let mut stream = connect(served.addr).await;
    get_request(&mut stream, "/", false).await;
    assert!(is_ok(&read_response(&mut stream).await));
    {
        let _cut = CutOnDrop::new(&served.control);
        let mut disarmed = CutOnDrop::new(&served.control);
        disarmed.disarm();
    }
    closed(&mut stream).await;
    served.control.stop();
    within(served.server).await.unwrap().unwrap();
}

// Not upstream's: two loops sharing a control are paused, released and
// counted together.
#[tokio::test]
async fn loops_sharing_a_control_move_together() {
    let control = ServeControl::new();
    let mut addrs = Vec::new();
    let mut servers = Vec::new();
    for _ in 0..2 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        addrs.push(listener.local_addr().unwrap());
        servers.push(tokio::spawn(run(
            listener,
            None,
            app(Gate::default()),
            &control,
        )));
    }
    let mut streams = Vec::new();
    for addr in &addrs {
        let mut stream = connect(*addr).await;
        get_request(&mut stream, "/", false).await;
        assert!(is_ok(&read_response(&mut stream).await));
        streams.push(stream);
    }
    assert_eq!(control.live_count(), 2);
    within(control.pause_accept()).await;
    assert_eq!(control.stats().accepting, 0);
    within(control.resume_accept()).await.unwrap();
    assert_eq!(control.stats().accepting, 2);

    let drained = control.release();
    for stream in &mut streams {
        closed(stream).await;
    }
    within(drained).await;
    for server in servers {
        within(server).await.unwrap().unwrap();
    }
}

// Not upstream's: a cut closes a WebSocket whose handler holds the socket
// and waits on something else, so nothing polls the IO. The peer sees the
// close, the connection leaves the count, the release completes, and a
// second cut finds nothing.
#[tokio::test]
async fn a_cut_closes_a_websocket_whose_handler_is_not_polling_it() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
    let (mut socket, _) = within(tokio_tungstenite::connect_async(format!(
        "ws://{}/hold",
        served.addr
    )))
    .await
    .unwrap();
    until(|| served.control.upgraded_count() == 1).await;

    let drained = served.control.release();
    tokio::pin!(drained);
    assert!(stays_pending(&mut drained).await);
    assert_eq!(served.control.cut(), 1);
    assert!(matches!(within(socket.next()).await, None | Some(Err(_))));
    assert_eq!(served.control.live_count(), 0);
    assert_eq!(served.control.upgraded_count(), 0);
    within(drained).await;
    assert_eq!(served.control.cut(), 0);
    within(served.server).await.unwrap().unwrap();
}

// Not upstream's: the same over HTTP/2 extended CONNECT.
#[tokio::test]
async fn a_cut_closes_an_http2_websocket_whose_handler_is_not_polling_it() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
    let (mut sender, driver) = h2_client(connect(served.addr).await).await;
    let request = hyper::Request::builder()
        .method(hyper::Method::CONNECT)
        .uri(format!("http://{}/hold", served.addr))
        .header("sec-websocket-version", "13")
        .extension(hyper::ext::Protocol::from_static("websocket"))
        .body(Body::empty())
        .unwrap();
    let mut response = within(sender.send_request(request)).await.unwrap();
    assert_eq!(response.status(), 200);
    let upgraded = within(hyper::upgrade::on(&mut response)).await.unwrap();
    let mut socket =
        WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Client, None).await;
    until(|| served.control.upgraded_count() == 1).await;

    let drained = served.control.release();
    tokio::pin!(drained);
    assert!(stays_pending(&mut drained).await);
    assert_eq!(served.control.cut(), 1);
    assert!(matches!(within(socket.next()).await, None | Some(Err(_))));
    let _ = within(driver).await;
    assert_eq!(served.control.live_count(), 0);
    within(drained).await;
    assert_eq!(served.control.cut(), 0);
}

// Not upstream's: pauses and resumes issued together neither hang nor leave
// the loop in a mode its listener doesn't match.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_pauses_and_resumes_settle() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
    for _ in 0..300 {
        let (pause, resume) = (served.control.clone(), served.control.clone());
        let pausing = tokio::spawn(async move { pause.pause_accept().await });
        let resuming = tokio::spawn(async move { resume.resume_accept().await });
        within(pausing).await.unwrap();
        within(resuming).await.unwrap().unwrap();
        let mode = *served.control.shared.mode.borrow();
        let accepting = served.control.stats().accepting;
        match mode {
            Mode::Running => assert_eq!(accepting, 1),
            Mode::Paused => assert_eq!(accepting, 0),
            other => panic!("left in {other:?}"),
        }
    }
    within(served.control.pause_accept()).await;
    assert_eq!(served.control.stats().accepting, 0);
    within(served.control.resume_accept()).await.unwrap();
    assert_eq!(served.control.stats().accepting, 1);
    served.control.stop();
    within(served.server).await.unwrap().unwrap();
}

// Not upstream's: a connection queued while the loop is paused is not
// accepted before the resume, however the pause came about.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_is_accepted_after_a_pause_returns() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
    for _ in 0..60 {
        within(served.control.pause_accept()).await;
        let live = served.control.live_count();
        let mut waiting = connect(served.addr).await;
        get_request(&mut waiting, "/", true).await;
        let early = tokio::time::timeout(Duration::from_millis(25), read_response(&mut waiting));
        assert!(early.await.is_err(), "served while paused");
        assert!(served.control.live_count() <= live, "accepted while paused");
        within(served.control.resume_accept()).await.unwrap();
        assert!(is_ok(&read_response(&mut waiting).await));
    }
    served.control.stop();
    within(served.server).await.unwrap().unwrap();
}

// Not upstream's: a resume after a release fails and a pause returns.
#[tokio::test]
async fn pause_and_resume_end_when_the_mode_moves_on() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
    let drained = served.control.release();
    within(served.control.pause_accept()).await;
    assert!(within(served.control.resume_accept()).await.is_err());
    within(drained).await;
    within(served.server).await.unwrap().unwrap();
}

// Not upstream's: a resume after a cut fails; the cut finished the control.
#[tokio::test]
async fn a_resume_after_a_cut_fails() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
    within(served.control.pause_accept()).await;
    assert_eq!(served.control.cut(), 0);
    assert!(within(served.control.resume_accept()).await.is_err());
    served.control.stop();
    within(served.server).await.unwrap().unwrap();
}

// Not upstream's: a loop that can't make its listener again ends and leaves
// the counts, which then look settled, and the resume still fails.
#[tokio::test]
async fn a_resume_fails_when_a_loop_cannot_make_its_listener_again() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
    within(served.control.pause_accept()).await;
    served
        .control
        .shared
        .fail_next_listener
        .store(true, Ordering::Release);
    let resumed = within(served.control.resume_accept()).await;
    assert!(
        resumed
            .as_ref()
            .is_err_and(|error| error.to_string().contains("couldn't accept again")),
        "{resumed:?}"
    );
    assert!(within(served.server).await.unwrap().is_err());
}

// Not upstream's: a loop polled for the first time just after a resume's
// wait settled counts itself as started before it has its listener. The
// resume judges the counts that settled it, so it succeeds, and the loop
// makes its listener on its own.
#[tokio::test]
async fn a_loop_started_after_the_resume_settled_does_not_fail_it() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
    within(served.control.pause_accept()).await;
    served
        .control
        .shared
        .start_a_loop_after_the_resume_settles
        .store(true, Ordering::Release);
    within(served.control.resume_accept()).await.unwrap();
    let mut client = connect(served.addr).await;
    get_request(&mut client, "/", true).await;
    assert!(is_ok(&read_response(&mut client).await));
    served.control.stop();
    within(served.server).await.unwrap().unwrap();
}

// Not upstream's: WebSockets that a release can't retire connect while the
// control is released and cut. None can escape the cut: afterwards every
// one is closed (EOF or a reset for its client) and the count is 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connections_registering_during_a_cut_do_not_escape_it() {
    for round in 0..15 {
        let served = start(None, FIRST_REQUEST_DEADLINE);
        let running = Arc::new(AtomicBool::new(true));
        let mut clients = Vec::new();
        for _ in 0..6 {
            let (addr, running) = (served.addr, running.clone());
            clients.push(tokio::spawn(async move {
                let mut held = Vec::new();
                while running.load(Ordering::Relaxed) {
                    if let Ok(Ok(mut stream)) =
                        tokio::time::timeout(Duration::from_millis(200), TcpStream::connect(addr))
                            .await
                    {
                        let _ = stream
                            .write_all(
                                b"GET /hold HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
                            )
                            .await;
                        held.push(stream);
                    }
                    tokio::task::yield_now().await;
                }
                held
            }));
        }
        until(|| served.control.live_count() > 3 + round % 4).await;
        let drained = served.control.release();
        served.control.cut();
        running.store(false, Ordering::Relaxed);
        let mut held = Vec::new();
        for client in clients {
            held.extend(within(client).await.unwrap());
        }
        // Whatever registered after the cut was cut as it did.
        until(|| served.control.live_count() == 0).await;
        within(drained).await;
        within(served.server).await.unwrap().unwrap();
        assert_eq!(served.control.live_count(), 0);
        // Each connection that was accepted ends in EOF or a reset. Ones
        // still in the backlog when the listeners went are reset too.
        for stream in &mut held {
            closed(stream).await;
        }
    }
}

// Not upstream's: the first-request deadline counts from the release, not
// from the end of a TLS handshake that was held up: a client that finishes
// its handshake before the deadline and sends nothing is closed at the
// deadline. A fresh deadline after the handshake would close it a second or
// more later, which the upper bound tells apart.
#[tokio::test]
async fn a_late_tls_handshake_does_not_get_a_fresh_deadline() {
    let (config, client, _dir) = certificates();
    let deadline = Duration::from_secs(3);
    let served = start(Some(config), deadline);
    let stalled = connect(served.addr).await;
    until(|| served.control.live_count() > 0).await;

    let released = Instant::now();
    let drained = served.control.release();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
    let mut stream = tls_connect(&connector, stalled).await;
    let handshaken = released.elapsed();
    closed(&mut stream).await;
    let elapsed = released.elapsed();
    // Only meaningful if the handshake ended well before the deadline.
    assert!(
        handshaken < deadline - Duration::from_millis(500),
        "the handshake was late: {handshaken:?}"
    );
    assert!(
        elapsed >= deadline - Duration::from_millis(100),
        "{elapsed:?}"
    );
    assert!(
        elapsed < deadline + Duration::from_secs(1),
        "a fresh deadline: {elapsed:?}"
    );
    within(drained).await;
}

// Not upstream's: a handshake still running at the deadline ends there.
#[tokio::test]
async fn a_tls_handshake_still_running_at_the_deadline_ends() {
    let (config, _client, _dir) = certificates();
    let deadline = Duration::from_millis(500);
    let served = start(Some(config), deadline);
    let mut stalled = connect(served.addr).await;
    until(|| served.control.live_count() > 0).await;
    let released = Instant::now();
    let drained = served.control.release();
    closed(&mut stalled).await;
    let elapsed = released.elapsed();
    assert!(
        elapsed >= deadline - Duration::from_millis(100),
        "{elapsed:?}"
    );
    within(drained).await;
}

// Not upstream's: a request that has reached the service is not cancelled
// by the first-request deadline, whenever in the deadline it arrives and
// however long its handler takes. (Paused time isn't workable here: with
// real sockets the clock would jump whenever the runtime waits for a
// packet. The race needs both the request and the timer ready at one poll,
// so this sweeps the arrival around the deadline.) A request the handler
// saw is always answered, and some iterations must have run one.
#[tokio::test]
async fn the_deadline_does_not_cancel_a_request_that_arrived() {
    let deadline = Duration::from_millis(150);
    let mut answered = 0;
    for offset in (0..=180).step_by(10) {
        let served = start(None, deadline);
        let mut stream = connect(served.addr).await;
        until(|| served.control.live_count() > 0).await;
        let drained = served.control.release();
        tokio::time::sleep(Duration::from_millis(offset)).await;
        get_request(&mut stream, "/slow", false).await;
        let arrived = tokio::select! {
            biased;
            () = served.gate.started.notified() => true,
            () = closed(&mut stream) => false,
        };
        if arrived {
            // Well past the deadline, with the handler still working.
            tokio::time::sleep(deadline * 2).await;
            served.gate.proceed.notify_one();
            let response = read_response(&mut stream).await;
            assert!(is_ok(&response), "offset {offset}: {response:?}");
            assert_eq!(response.1, "done");
            answered += 1;
        }
        closed(&mut stream).await;
        within(drained).await;
    }
    assert!(answered > 0, "no iteration ran a request that arrived");
}

// Not upstream's: a loop whose future is made and never polled holds no
// listener, so a pause and a resume don't wait for it, and a second pause
// doesn't hang behind either. Polled, it serves; a release waits for it.
#[tokio::test]
async fn an_unpolled_loop_does_not_strand_a_resume() {
    let control = ServeControl::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let serving = run(listener, None, app(Gate::default()), &control);

    within(control.pause_accept()).await;
    within(control.resume_accept()).await.unwrap();
    within(control.pause_accept()).await;
    within(control.resume_accept()).await.unwrap();

    let server = tokio::spawn(serving);
    let mut stream = connect(addr).await;
    get_request(&mut stream, "/", true).await;
    let response = read_response(&mut stream).await;
    assert!(is_ok(&response), "{response:?}");

    let drained = control.release();
    within(drained).await;
    within(server).await.unwrap().unwrap();
}

// Not upstream's: a release can't pass a loop that hasn't been polled yet.
#[tokio::test]
async fn a_release_waits_for_a_loop_that_has_not_started() {
    let control = ServeControl::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let serving = run(listener, None, app(Gate::default()), &control);
    let drained = control.release();
    tokio::pin!(drained);
    assert!(stays_pending(&mut drained).await);
    within(serving).await.unwrap();
    within(drained).await;
}

// Not upstream's: a cut while running stops the loop and finishes the
// control: the run future completes, a resume fails and nothing more is
// accepted.
#[tokio::test]
async fn a_cut_while_running_stops_accepting() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
    let mut stream = connect(served.addr).await;
    get_request(&mut stream, "/", false).await;
    assert!(is_ok(&read_response(&mut stream).await));
    assert_eq!(served.control.cut(), 1);
    within(served.server).await.unwrap().unwrap();
    assert!(within(served.control.resume_accept()).await.is_err());
    assert_eq!(served.control.stats().accepting, 0);
    // The socket is closed with the loop, so no one connects.
    let refused = tokio::time::timeout(WAIT, TcpStream::connect(served.addr)).await;
    assert!(!matches!(refused, Ok(Ok(_))), "connected after a cut");
}

// Not upstream's: a cut during a resume neither hangs it nor leaves the
// control resumable: the resume ends (Err if the cut landed before it
// finished, Ok if after), and every later resume fails. This repeats the
// race; a loop held before it builds its listener would pin the cut to the
// wait, but needs a hook in the loop, so the Err of the pinned case isn't
// asserted. `a_cut_while_running_stops_accepting` covers the cut's mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cut_during_a_resume_fails_it() {
    for _ in 0..100 {
        let served = start(None, FIRST_REQUEST_DEADLINE);
        within(served.control.pause_accept()).await;
        let control = served.control.clone();
        let resuming = tokio::spawn(async move { control.resume_accept().await });
        served.control.cut();
        // It may have finished before the cut landed, and then it is Ok;
        // it never hangs, and once cut, every resume fails.
        let _ = within(resuming).await.unwrap();
        assert!(within(served.control.resume_accept()).await.is_err());
        within(served.server).await.unwrap().unwrap();
    }
}

// Not upstream's: a duplicate of an accepted socket that fails drops the
// connection and makes the loop wait as after an accept error, so the next
// client isn't accepted at once; it is served after the wait.
#[tokio::test]
async fn a_failed_duplicate_takes_the_accept_error_wait() {
    let served = start(None, FIRST_REQUEST_DEADLINE);
    served
        .control
        .shared
        .fail_next_duplicate
        .store(true, Ordering::Release);
    let mut dropped = connect(served.addr).await;
    let _ = dropped
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await;
    closed(&mut dropped).await;

    let asked = Instant::now();
    let mut next = connect(served.addr).await;
    get_request(&mut next, "/", true).await;
    assert!(
        stays_pending(read_response(&mut next)).await,
        "served during the wait"
    );
    let response = read_response(&mut next).await;
    assert!(is_ok(&response), "{response:?}");
    assert!(
        asked.elapsed() >= ACCEPT_ERROR_WAIT - Duration::from_millis(500),
        "{:?}",
        asked.elapsed()
    );
    served.control.stop();
    within(served.server).await.unwrap().unwrap();
}
