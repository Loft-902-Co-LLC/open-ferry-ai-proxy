use std::error::Error;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::future::BoxFuture;
use open_ferry_core::auth::{Auth, Status};
use open_ferry_core::exec::{Dispatcher, Options, Request, Response, StreamResponse};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::manager::{CallResult, ErrorEvents, Manager, Settings};
use open_ferry_core::models::ModelInfo;
use open_ferry_core::registry::ModelRegistry;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

use super::*;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Case {
    BeforeResponse,
    MidBody,
    Sending,
    GoAway,
    GoAwayError,
    Reset,
    ResetError,
    /// TLS + ALPN h2: 200 and some DATA, then TCP closes with no close_notify.
    TlsMidBody,
    /// TLS + ALPN h2: 200 and some DATA, then RST_STREAM(CANCEL).
    TlsReset,
    /// GOAWAY covers the active stream before an unclean TLS close.
    TlsGoAway,
    /// GOAWAY covers the request, then an unclean TLS close; no response at all.
    TlsGoAwayBeforeResponse,
}

impl Case {
    fn tls(self) -> bool {
        matches!(
            self,
            Case::TlsMidBody | Case::TlsReset | Case::TlsGoAway | Case::TlsGoAwayBeforeResponse
        )
    }
}

async fn headers(socket: &mut TcpStream) {
    let mut raw = Vec::new();
    while !raw.ends_with(b"\r\n\r\n") {
        raw.push(socket.read_u8().await.unwrap());
    }
}

async fn frame<S: AsyncRead + AsyncWrite + Unpin>(
    socket: &mut S,
    kind: u8,
    flags: u8,
    id: u32,
    payload: &[u8],
) {
    let length = (payload.len() as u32).to_be_bytes();
    socket.write_all(&length[1..]).await.unwrap();
    socket.write_all(&[kind, flags]).await.unwrap();
    socket.write_all(&id.to_be_bytes()).await.unwrap();
    socket.write_all(payload).await.unwrap();
}

/// Complete the h2 preface/SETTINGS exchange and return the id of the first
/// request stream.
async fn request<S: AsyncRead + AsyncWrite + Unpin>(socket: &mut S) -> u32 {
    let mut preface = [0u8; 24];
    socket.read_exact(&mut preface).await.unwrap();
    assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    frame(socket, 4, 0, 0, &[]).await;
    loop {
        let mut header = [0u8; 9];
        socket.read_exact(&mut header).await.unwrap();
        let length = u32::from_be_bytes([0, header[0], header[1], header[2]]) as usize;
        let mut payload = vec![0; length];
        socket.read_exact(&mut payload).await.unwrap();
        if header[3] == 4 && header[4] == 0 {
            frame(socket, 4, 1, 0, &[]).await;
            socket.flush().await.unwrap();
        }
        if header[3] == 1 {
            return u32::from_be_bytes(header[5..9].try_into().unwrap());
        }
    }
}

/// Self-signed loopback certificate: (DER for the client, rustls server config).
fn tls_identity() -> (Vec<u8>, Arc<rustls::ServerConfig>) {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let der = generated.cert.der().to_vec();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(generated.signing_key.serialize_der());
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![generated.cert.der().clone()], key.into())
    .unwrap();
    config.alpn_protocols = vec![b"h2".to_vec()];
    (der, Arc::new(config))
}

/// Serve one TLS HTTP/2 request: HEADERS (:status 200) and DATA "x" without
/// END_STREAM, wait until the client has read the DATA, then fail the stream.
async fn tls_http2(
    tcp: TcpStream,
    config: Arc<rustls::ServerConfig>,
    case: Case,
    ready: oneshot::Receiver<()>,
    done: oneshot::Receiver<()>,
) {
    let mut tls = tokio_rustls::TlsAcceptor::from(config)
        .accept(tcp)
        .await
        .unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    let id = request(&mut tls).await;
    if !matches!(case, Case::TlsGoAwayBeforeResponse) {
        frame(&mut tls, 1, 4, id, &[0x88]).await;
        frame(&mut tls, 0, 0, id, b"x").await;
        tls.flush().await.unwrap();
        ready.await.expect("client must read the DATA frame");
    }
    if matches!(case, Case::TlsReset) {
        frame(&mut tls, 3, 0, id, &8u32.to_be_bytes()).await;
        tls.flush().await.unwrap();
        // The connection stays up until the client has seen the reset.
        let _ = done.await;
    } else {
        if matches!(case, Case::TlsGoAway | Case::TlsGoAwayBeforeResponse) {
            let mut payload = id.to_be_bytes().to_vec();
            payload.extend(0u32.to_be_bytes());
            frame(&mut tls, 7, 0, 0, &payload).await;
            tls.flush().await.unwrap();
        }
        // FIN on the raw socket with no TLS close_notify. Only the write
        // side is shut down so unread client frames cannot turn it into RST.
        let (mut tcp, _) = tls.into_inner();
        tcp.shutdown().await.unwrap();
        let mut drain = Vec::new();
        tokio::select! {
            _ = done => {}
            _ = tcp.read_to_end(&mut drain) => {}
        }
    }
}

/// Serve one TLS HTTP/2 request whose upload never finishes: HEADERS (:status
/// 200) then DATA "x" with END_STREAM and no RST_STREAM. The response is
/// complete but the client's stream is only half-closed (remote), so the
/// connection can still fail it. Wait until the client has seen the end of the
/// body, then close the socket with no TLS close_notify.
async fn tls_http2_complete_response(
    tcp: TcpStream,
    config: Arc<rustls::ServerConfig>,
    ended: oneshot::Receiver<()>,
    done: oneshot::Receiver<()>,
) {
    let mut tls = tokio_rustls::TlsAcceptor::from(config)
        .accept(tcp)
        .await
        .unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    let id = request(&mut tls).await;
    frame(&mut tls, 1, 4, id, &[0x88]).await;
    frame(&mut tls, 0, 1, id, b"x").await;
    tls.flush().await.unwrap();
    ended
        .await
        .expect("client must read to the end of the body");
    // FIN with no close_notify; write side only, as in `tls_http2`.
    let (mut tcp, _) = tls.into_inner();
    tcp.shutdown().await.unwrap();
    let mut drain = Vec::new();
    tokio::select! {
        _ = done => {}
        _ = tcp.read_to_end(&mut drain) => {}
    }
}

/// Walk the source chain looking for an h2::Error whose get_io() is the
/// UnexpectedEof from the TLS read.
fn h2_unexpected_eof(error: &(dyn Error + 'static)) -> bool {
    let mut next = Some(error);
    while let Some(current) = next {
        if let Some(h2) = current.downcast_ref::<h2::Error>() {
            return h2
                .get_io()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof);
        }
        next = current.source();
    }
    false
}

/// Walk the source chain looking for any io::Error of kind UnexpectedEof.
fn io_unexpected_eof(error: &(dyn Error + 'static)) -> bool {
    let mut next = Some(error);
    while let Some(current) = next {
        if current
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof)
        {
            return true;
        }
        next = current.source();
    }
    false
}

/// Walk the source chain looking for any h2::Error.
fn h2_in_chain(error: &(dyn Error + 'static)) -> bool {
    let mut next = Some(error);
    while let Some(current) = next {
        if current.downcast_ref::<h2::Error>().is_some() {
            return true;
        }
        next = current.source();
    }
    false
}

async fn http2(socket: &mut TcpStream, case: Case) {
    let id = request(socket).await;
    match case {
        Case::GoAway | Case::GoAwayError => {
            let code: u32 = if matches!(case, Case::GoAway) { 0 } else { 2 };
            let mut payload = id.to_be_bytes().to_vec();
            payload.extend(code.to_be_bytes());
            frame(socket, 7, 0, 0, &payload).await;
        }
        Case::Reset | Case::ResetError => {
            let code: u32 = if matches!(case, Case::Reset) { 8 } else { 2 };
            frame(socket, 3, 0, id, &code.to_be_bytes()).await;
        }
        _ => unreachable!(),
    }
}

/// The transport error plus the context captured where the response arrived
/// (`None` when no response did, so the failure is a before-response one).
async fn capture(case: Case) -> (reqwest::Error, Option<ResponseBody>) {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let (done, wait) = oneshot::channel();
    let (ready, ready_wait) = oneshot::channel();
    let mut ready = Some(ready);
    let tls = case.tls().then(tls_identity);
    let server_config = tls.as_ref().map(|(_, config)| config.clone());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        match case {
            Case::TlsMidBody | Case::TlsReset | Case::TlsGoAway | Case::TlsGoAwayBeforeResponse => {
                tls_http2(socket, server_config.unwrap(), case, ready_wait, wait).await;
            }
            Case::BeforeResponse | Case::MidBody | Case::Sending => {
                headers(&mut socket).await;
                if matches!(case, Case::MidBody) {
                    socket
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nx")
                        .await
                        .unwrap();
                }
                if matches!(case, Case::Sending) {
                    socket2::SockRef::from(&socket)
                        .set_linger(Some(Duration::ZERO))
                        .unwrap();
                } else {
                    socket.shutdown().await.unwrap();
                }
            }
            _ => {
                http2(&mut socket, case).await;
                if matches!(case, Case::Reset | Case::ResetError) {
                    // A reset is a stream failure, not a connection close.
                    let _ = wait.await;
                } else {
                    socket.shutdown().await.unwrap();
                }
            }
        }
    });
    let builder = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5));
    let builder = match (case, &tls) {
        (_, Some((der, _))) => builder
            .tls_certs_only([reqwest::Certificate::from_der(der).unwrap()])
            .resolve("localhost", addr),
        (Case::BeforeResponse | Case::MidBody | Case::Sending, None) => builder.http1_only(),
        _ => builder.http2_prior_knowledge(),
    };
    let client = builder.build().unwrap();
    let request = if matches!(case, Case::Sending) {
        client
            .post(format!("http://{addr}"))
            .body(vec![0u8; 16 * 1024 * 1024])
    } else if case.tls() {
        client.get(format!("https://localhost:{}", addr.port()))
    } else {
        client.get(format!("http://{addr}"))
    };
    let (error, body) = match request.send().await {
        Ok(mut response) => {
            // Captured where the response arrives, before its body is read.
            let body = ResponseBody::new(&response);
            if case.tls() {
                assert_eq!(response.version(), reqwest::Version::HTTP_2);
                // Prove a real response and DATA arrived before the failure.
                let chunk = response.chunk().await.unwrap();
                assert_eq!(chunk.as_deref(), Some(&b"x"[..]));
                let _ = ready.take().unwrap().send(());
            }
            (response.bytes().await.unwrap_err(), Some(body))
        }
        Err(error) => (error, None),
    };
    // Release the fixture even if the client failed before reading DATA.
    drop(ready);
    let _ = done.send(());
    server.await.unwrap();
    assert!(
        !error.is_timeout(),
        "fixture timed out: {case:?}: {error:?}"
    );
    if matches!(case, Case::TlsMidBody | Case::TlsGoAway) {
        assert!(
            h2_unexpected_eof(&error),
            "expected h2::Error::get_io() UnexpectedEof in chain: {error:?}"
        );
    }
    if matches!(case, Case::TlsGoAwayBeforeResponse) {
        // The response-future path loses the h2 wrapper; the io error remains.
        assert!(
            io_unexpected_eof(&error),
            "expected io UnexpectedEof in chain: {error:?}"
        );
    }
    (error, body)
}

/// Run a real socket error through the shared mapper, then through the
/// manager's normal result recording. HTTP/2 uses h2c for response-future
/// failures and TLS with ALPN for response-body failures. The mapper also gets
/// the context captured when the response arrived: `Some` for a body failure
/// (use `ResponseBody::classify`), `None` for a before-response one (use the
/// plain `classify`).
pub(crate) async fn check(
    case: Case,
    map: impl FnOnce(reqwest::Error, Option<ResponseBody>) -> ExecError,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (error, body) = capture(case).await;
        let mapped = map(error, body);
        let want = match case {
            Case::BeforeResponse | Case::MidBody => "connection_lifecycle",
            Case::Sending | Case::GoAway | Case::GoAwayError => "transient_transport",
            // An HTTP/2 body EOF cannot be told apart from one that followed a
            // GOAWAY, and GOAWAY must not be promoted, so both stay unclassified.
            Case::TlsMidBody | Case::TlsGoAway | Case::TlsGoAwayBeforeResponse => "",
            Case::Reset | Case::ResetError | Case::TlsReset => "",
        };
        // GOAWAY followed by EOF is already a transient text match in Rust:
        // h2 reports BrokenPipe, not Go's GoAwayError. It must not turn into
        // a lifecycle drop. Stream resets must remain unclassified.
        if matches!(
            case,
            Case::GoAway | Case::GoAwayError | Case::Reset | Case::ResetError | Case::TlsReset
        ) {
            assert_ne!(
                mapped.transport,
                Some(TransportFault::Lifecycle),
                "{mapped:?}"
            );
        }
        let result = recorded(mapped).await;
        let error = result
            .error
            .as_ref()
            .expect("failed result must have an error");
        assert_eq!(error.code, want, "{case:?}: {error:?}");
    })
    .await
    .expect("local transport fixture timed out");
}

/// Hand `failure` to the manager as an executor error and return the result
/// the manager published for it. Lifecycle/transient failures deliberately
/// leave no credential error, so the classified result is observed through the
/// manager's public event hook.
async fn recorded(failure: ExecError) -> CallResult {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_client(
        "closed-connection",
        "local-transport",
        &[ModelInfo {
            id: "local-model".into(),
            ..ModelInfo::default()
        }],
    );
    let manager = Manager::new(Settings::default(), registry, None);
    let events = Arc::new(Failures::default());
    manager.set_error_events(events.clone());
    manager.register_executor(Arc::new(Failed(failure)));
    manager
        .register(Auth {
            id: "closed-connection".into(),
            provider: "local-transport".into(),
            status: Status::Active,
            ..Auth::default()
        })
        .unwrap();
    let _ = manager
        .execute(
            &["local-transport".into()],
            Request {
                model: "local-model".into(),
                payload: Bytes::from_static(b"{}"),
            },
            Options::new("openai".into()),
        )
        .await;
    let published = events.0.lock().unwrap();
    published
        .last()
        .expect("manager must publish the failure")
        .clone()
}

#[derive(Default)]
struct Failures(Mutex<Vec<CallResult>>);

impl ErrorEvents for Failures {
    fn publish(&self, result: &CallResult, _: &Auth) {
        self.0.lock().unwrap().push(result.clone());
    }
}

struct Failed(ExecError);
impl ProviderExecutor for Failed {
    fn id(&self) -> &str {
        "local-transport"
    }
    fn execute(
        &self,
        _: Arc<Auth>,
        _: Request,
        _: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        Box::pin(async { Err(self.0.clone()) })
    }
    fn execute_stream(
        &self,
        _: Arc<Auth>,
        _: Request,
        _: Options,
    ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
        Box::pin(async { Err(self.0.clone()) })
    }
    fn count_tokens(
        &self,
        _: Arc<Auth>,
        _: Request,
        _: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        Box::pin(async { Err(self.0.clone()) })
    }
    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        Box::pin(async move { Ok((*auth).clone()) })
    }
}

macro_rules! http_error_tests {
    ($map:expr) => {
        #[tokio::test]
        async fn closed_before_response() {
            crate::transport::tests::check(crate::transport::tests::Case::BeforeResponse, $map)
                .await;
        }
        #[tokio::test]
        async fn closed_mid_body() {
            crate::transport::tests::check(crate::transport::tests::Case::MidBody, $map).await;
        }
        #[tokio::test]
        async fn closed_while_sending() {
            crate::transport::tests::check(crate::transport::tests::Case::Sending, $map).await;
        }
        #[tokio::test]
        async fn goaway_is_not_a_lifecycle_drop() {
            crate::transport::tests::check(crate::transport::tests::Case::GoAway, $map).await;
            crate::transport::tests::check(crate::transport::tests::Case::GoAwayError, $map).await;
        }
        #[tokio::test]
        async fn stream_reset_is_not_a_closed_connection() {
            crate::transport::tests::check(crate::transport::tests::Case::Reset, $map).await;
            crate::transport::tests::check(crate::transport::tests::Case::ResetError, $map).await;
        }
        #[tokio::test]
        async fn tls_http2_mid_body_eof_is_ambiguous_and_stays_unclassified() {
            // Without close_notify the body EOF may or may not have followed a
            // GOAWAY; the error cannot say, so it is retained unchanged.
            crate::transport::tests::check(crate::transport::tests::Case::TlsMidBody, $map).await;
        }
        #[tokio::test]
        async fn tls_http2_goaway_before_response_then_unclean_close() {
            crate::transport::tests::check(
                crate::transport::tests::Case::TlsGoAwayBeforeResponse,
                $map,
            )
            .await;
        }
        #[tokio::test]
        async fn tls_http2_stream_reset_is_not_a_closed_connection() {
            crate::transport::tests::check(crate::transport::tests::Case::TlsReset, $map).await;
        }
        #[tokio::test]
        async fn tls_http2_goaway_then_unclean_close() {
            crate::transport::tests::check(crate::transport::tests::Case::TlsGoAway, $map).await;
        }
    };
}
pub(crate) use http_error_tests;

crate::transport::tests::http_error_tests!(|error: reqwest::Error, body: Option<ResponseBody>| {
    let error = error.without_url();
    let failure = ExecError::upstream(0, crate::codex::client::error_chain(&error));
    match body {
        Some(body) => body.classify(&error, failure),
        None => classify(&error, failure),
    }
});

#[test]
fn statuses_and_unrelated_io_errors_are_not_reclassified() {
    let error = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "body ended");
    let failure = ExecError::upstream(429, "quota");
    assert_eq!(classify(&error, failure).transport, None);
    for kind in [
        std::io::ErrorKind::InvalidData,
        std::io::ErrorKind::PermissionDenied,
        std::io::ErrorKind::Other,
    ] {
        let error = std::io::Error::new(kind, "channel closed");
        assert_eq!(
            classify(&error, ExecError::upstream(0, error.to_string())).transport,
            None
        );
    }
}

#[tokio::test]
async fn closed_dispatcher_is_typed_not_a_text_match() {
    use hyper_util::rt::{TokioExecutor, TokioIo};
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let socket = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (peer, _) = listener.accept().await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http2::handshake::<_, _, reqwest::Body>(
        TokioExecutor::new(),
        TokioIo::new(socket),
    )
    .await
    .unwrap();
    drop(peer);
    tokio::spawn(connection).await.unwrap().ok();
    let error = sender.ready().await.unwrap_err();
    assert_eq!(error.to_string(), "channel closed");
    #[derive(Debug)]
    struct Wrapped(hyper::Error);
    impl std::fmt::Display for Wrapped {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("client error (Connect)")
        }
    }
    impl Error for Wrapped {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(&self.0)
        }
    }
    let failure = classify(
        &Wrapped(error),
        ExecError::upstream(0, "client error (Connect): channel closed"),
    );
    assert_eq!(failure.transport, Some(TransportFault::Lifecycle));
    let text = std::io::Error::other("channel closed");
    assert_eq!(
        classify(&text, ExecError::upstream(0, text.to_string())).transport,
        None
    );
}

// Everything below is the trailer-phase regression. Its tests are all named
// `trailer_regression*` so one `--skip trailer_regression` leaves out exactly
// these and keeps the original suite's counts.

/// Try to reproduce the wrapper-less HTTP/2 error end to end: a complete
/// response while the upload is still open, an unclean TLS close, then another
/// poll of the already-finished body. In that state only a connection I/O
/// error reaches the stream, and hyper reports it on the trailer path through
/// `new_h2`, which drops the `h2::Error` wrapper for I/O errors. Returns the
/// error and the context captured when the response arrived.
///
/// This depends on stream-state timing inside h2/hyper/reqwest. The caller
/// asserts that the error really has the intended shape, so a fixture that
/// cannot reach the path fails loudly instead of passing for nothing.
async fn capture_trailer() -> (reqwest::Error, ResponseBody) {
    use futures_util::StreamExt;

    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let (identity, config) = tls_identity();
    let (ended, ended_wait) = oneshot::channel();
    let (done, done_wait) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        tls_http2_complete_response(socket, config, ended_wait, done_wait).await;
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .tls_certs_only([reqwest::Certificate::from_der(&identity).unwrap()])
        .resolve("localhost", addr)
        .build()
        .unwrap();
    // One chunk, then never any more and never an end: the request stays open
    // after the response completes, which leaves the stream half-closed.
    let first: Result<Bytes, std::io::Error> = Ok(Bytes::from_static(b"x"));
    let upload = futures_util::stream::iter([first]).chain(futures_util::stream::pending());
    let mut response = client
        .post(format!("https://localhost:{}", addr.port()))
        .body(reqwest::Body::wrap_stream(upload))
        .send()
        .await
        .unwrap();
    let body = ResponseBody::new(&response);
    assert_eq!(response.version(), reqwest::Version::HTTP_2);
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let chunk = response.chunk().await.unwrap();
    assert_eq!(chunk.as_deref(), Some(&b"x"[..]));
    // END_STREAM: the body ends cleanly while the connection is still up.
    assert_eq!(response.chunk().await.unwrap(), None);
    let _ = ended.send(());
    // Poll the finished body again until the lost connection shows up.
    let error = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match response.chunk().await {
                Err(error) => break error,
                Ok(None) => tokio::time::sleep(Duration::from_millis(5)).await,
                Ok(Some(chunk)) => panic!("data after the end of the body: {chunk:?}"),
            }
        }
    })
    .await
    .expect("the finished body never reported the lost connection (fixture not reproducible)");
    let _ = done.send(());
    server.await.unwrap();
    assert!(!error.is_timeout(), "fixture timed out: {error:?}");
    (error, body)
}

/// An HTTP/2 response that ended cleanly and then lost its connection reaches
/// the caller as a bare io `UnexpectedEof` with no `h2::Error` in the chain,
/// exactly what an HTTP/1 body cut short looks like. The captured HTTP version
/// is the only thing that tells them apart, so this must not be promoted.
#[tokio::test]
async fn trailer_regression() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (error, body) = capture_trailer().await;
        // The test only means something if the fixture produced that shape.
        assert!(
            io_unexpected_eof(&error) && !h2_in_chain(&error),
            "fixture did not produce a bare io UnexpectedEof: {error:?}"
        );
        let error = error.without_url();
        let failure = ExecError::upstream(0, crate::codex::client::error_chain(&error));
        let mapped = body.classify(&error, failure);
        assert_eq!(mapped.transport, None, "{mapped:?}");
        let result = recorded(mapped).await;
        let error = result
            .error
            .as_ref()
            .expect("failed result must have an error");
        assert_eq!(error.code, "", "{error:?}");
    })
    .await
    .expect("local trailer fixture timed out");
}

/// The same wrapper-less error with only the response version changed: a
/// positively HTTP/1 response is promoted, HTTP/2 and HTTP/3 are not. The
/// responses are real `reqwest::Response` values, so `ResponseBody::new` is
/// what reads the version. This holds even where the fixture above cannot
/// reach the race.
#[test]
fn trailer_regression_version_rule() {
    let response = |version: reqwest::Version| {
        reqwest::Response::from(
            hyper::Response::builder()
                .version(version)
                .status(200)
                .body("")
                .unwrap(),
        )
    };
    let error = std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "peer closed connection without sending TLS close_notify",
    );
    let transport = |version: reqwest::Version| {
        ResponseBody::new(&response(version))
            .classify(&error, ExecError::upstream(0, error.to_string()))
            .transport
    };
    assert_eq!(transport(reqwest::Version::HTTP_2), None);
    assert_eq!(transport(reqwest::Version::HTTP_3), None);
    assert_eq!(
        transport(reqwest::Version::HTTP_11),
        Some(TransportFault::ResponseBody)
    );
}
