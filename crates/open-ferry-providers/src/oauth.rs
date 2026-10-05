// Ported from CLIProxyAPI internal/auth/codex/pkce.go and oauth_server.go,
// which internal/auth/claude repeats (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What every OAuth login needs: PKCE codes, a `state` value, and a local
//! server for the provider's redirect.
//!
//! Deviations from upstream:
//! - The callback server listens on 127.0.0.1 only; upstream listens on
//!   every interface.
//! - The success page is this project's own and takes nothing from the
//!   query; upstream's copies a `platform_url` parameter into its HTML
//!   unescaped.
//! - A callback whose `state` isn't the login's, an error report included,
//!   is turned away with 400 and doesn't end the wait. Upstream hands a code
//!   on and checks its state later, and takes an error report as the
//!   login's without checking it.
//! - Upstream's 10-second read and write deadlines are one deadline here: a
//!   connection is closed once it has gone 10 seconds since connecting, or
//!   since its last response, without sending a whole request, or without
//!   taking the response it is owed.
//! - A stopped server gives its open connections 5 seconds (upstream's
//!   shutdown timeout) to finish and then closes them; upstream's shutdown
//!   gives up waiting and leaves them to their deadlines.

use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::StatusCode;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};
use tokio::time::{Instant, Sleep};

/// A PKCE verifier and its S256 challenge (upstream's `PKCECodes`).
#[derive(Clone)]
pub struct Pkce {
    /// Sent with the code exchange.
    pub verifier: String,
    /// Sent with the authorization request.
    pub challenge: String,
}

impl Pkce {
    /// A fresh pair: 96 random bytes as unpadded URL-safe base64, and its
    /// SHA-256 the same way.
    pub fn generate() -> Self {
        let verifier = URL_SAFE_NO_PAD.encode(random_bytes::<96>());
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self {
            verifier,
            challenge,
        }
    }
}

/// A fresh `state` value for an authorization request.
pub fn generate_state() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes::<32>())
}

fn random_bytes<const N: usize>() -> [u8; N] {
    rand::random()
}

/// What came back to the redirect URI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallbackResult {
    /// The authorization code.
    Code(String),
    /// The provider's `error` parameter, or `no_code`.
    Error(String),
}

/// Why waiting for the callback failed.
#[derive(Debug)]
pub enum CallbackError {
    /// No callback came in time.
    Timeout,
    /// The server stopped before one came.
    Closed,
}

impl std::fmt::Display for CallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Timeout => "timeout waiting for OAuth callback",
            Self::Closed => "the OAuth callback server stopped",
        })
    }
}

impl std::error::Error for CallbackError {}

/// A local server for a provider's OAuth redirect (upstream's
/// `OAuthServer`). It stops when dropped.
pub struct CallbackServer {
    port: u16,
    result: oneshot::Receiver<CallbackResult>,
    shutdown: watch::Sender<bool>,
}

#[derive(Clone)]
struct CallbackState {
    state: Arc<str>,
    result: Arc<Mutex<Option<oneshot::Sender<CallbackResult>>>>,
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// How long a connection has to send a request, or take a response
/// (upstream's `ReadTimeout` and `WriteTimeout`).
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a stopped server lets open connections finish (upstream's
/// shutdown timeout in `Stop`).
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// The callback server's time limits.
#[derive(Clone, Copy, Debug)]
struct Limits {
    connection: Duration,
    shutdown: Duration,
}

impl Limits {
    const DEFAULT: Self = Self {
        connection: CONNECTION_TIMEOUT,
        shutdown: SHUTDOWN_GRACE,
    };
}

impl CallbackServer {
    /// Listens on 127.0.0.1:`port` (0 for any free port) for a redirect to
    /// `path` that carries `state`.
    pub async fn start(port: u16, path: &str, state: &str) -> io::Result<Self> {
        Self::start_with(port, path, state, Limits::DEFAULT).await
    }

    async fn start_with(port: u16, path: &str, state: &str, limits: Limits) -> io::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await?;
        let port = listener.local_addr()?.port();
        let (sender, result) = oneshot::channel();
        let app = Router::new()
            .route(path, get(callback))
            .route("/success", get(success))
            .with_state(CallbackState {
                state: state.into(),
                result: Arc::new(Mutex::new(Some(sender))),
            });
        let (shutdown, stopped) = watch::channel(false);
        let listener = CallbackListener {
            inner: listener,
            stopped: stopped.clone(),
            limits,
        };
        tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app)
                .with_graceful_shutdown(stop_signal(stopped))
                .await
            {
                tracing::warn!("OAuth callback server failed: {error}");
            }
        });
        Ok(Self {
            port,
            result,
            shutdown,
        })
    }

    /// The port it listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Waits up to `timeout` for the redirect.
    pub async fn wait(&mut self, timeout: Duration) -> Result<CallbackResult, CallbackError> {
        match tokio::time::timeout(timeout, &mut self.result).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err(CallbackError::Closed),
            Err(_) => Err(CallbackError::Timeout),
        }
    }
}

impl Drop for CallbackServer {
    /// Stops taking connections, and closes the open ones once they have
    /// finished or five seconds have passed.
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

/// Resolves once the server is told to stop, or its owner is gone.
async fn stop_signal(mut stopped: watch::Receiver<bool>) {
    let _ = stopped.wait_for(|stop| *stop).await;
}

/// The callback server's listener: a TCP listener whose connections are held
/// to the server's [`Limits`].
struct CallbackListener {
    inner: TcpListener,
    stopped: watch::Receiver<bool>,
    limits: Limits,
}

impl axum::serve::Listener for CallbackListener {
    type Io = LimitedConnection;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (stream, address) = axum::serve::Listener::accept(&mut self.inner).await;
        let connection = LimitedConnection::new(stream, self.stopped.clone(), self.limits);
        (connection, address)
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// A connection that fails, and so is closed, once it has gone the
/// connection timeout without a whole request or without taking its
/// response, or once the server has stopped and the shutdown grace has
/// passed. A request still being read when the server stops is cut off then,
/// as no graceful shutdown would end it.
struct LimitedConnection {
    stream: TcpStream,
    timeout: Duration,
    /// When the connection times out: [`Limits::connection`] after it was
    /// accepted or last wrote.
    deadline: Pin<Box<Sleep>>,
    /// Resolves the shutdown grace after the server stops.
    closing: Pin<Box<dyn Future<Output = ()> + Send>>,
    /// Why the connection failed, once it has.
    failed: Option<io::ErrorKind>,
}

impl LimitedConnection {
    fn new(stream: TcpStream, stopped: watch::Receiver<bool>, limits: Limits) -> Self {
        Self {
            stream,
            timeout: limits.connection,
            deadline: Box::pin(tokio::time::sleep(limits.connection)),
            closing: Box::pin(async move {
                stop_signal(stopped).await;
                tokio::time::sleep(limits.shutdown).await;
            }),
            failed: None,
        }
    }

    /// Fails once the connection has timed out or must close. Polling the
    /// deadline and the shutdown wakes the connection when either comes,
    /// even while the client sends nothing.
    fn check(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if self.failed.is_none() {
            if self.closing.as_mut().poll(cx).is_ready() {
                self.failed = Some(io::ErrorKind::ConnectionAborted);
            } else if self.deadline.as_mut().poll(cx).is_ready() {
                self.failed = Some(io::ErrorKind::TimedOut);
            }
        }
        match self.failed {
            Some(io::ErrorKind::TimedOut) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "OAuth callback connection timed out",
            )),
            Some(kind) => Err(io::Error::new(kind, "OAuth callback server stopped")),
            None => Ok(()),
        }
    }
}

impl AsyncRead for LimitedConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.check(cx)?;
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for LimitedConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.check(cx)?;
        let written = Pin::new(&mut self.stream).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = written
            && n > 0
        {
            // A response went out; the next request gets a fresh deadline.
            let next = Instant::now() + self.timeout;
            self.deadline.as_mut().reset(next);
        }
        written
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.check(cx)?;
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

async fn callback(
    State(server): State<CallbackState>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let nonempty = |value: Option<String>| value.filter(|value| !value.is_empty());
    // Nothing without the login's state counts, not even an error report:
    // anyone who can reach the port could otherwise end the login.
    if nonempty(query.state).as_deref() != Some(&*server.state) {
        return (StatusCode::BAD_REQUEST, "State parameter doesn't match").into_response();
    }
    let (result, response) = if let Some(error) = nonempty(query.error) {
        let response = (StatusCode::BAD_REQUEST, format!("OAuth error: {error}")).into_response();
        (CallbackResult::Error(error), response)
    } else if let Some(code) = nonempty(query.code) {
        let response = (StatusCode::FOUND, [(http::header::LOCATION, "/success")]).into_response();
        (CallbackResult::Code(code), response)
    } else {
        let response = (StatusCode::BAD_REQUEST, "No authorization code received").into_response();
        (CallbackResult::Error("no_code".to_owned()), response)
    };
    let sender = server
        .result
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some(sender) = sender {
        let _ = sender.send(result);
    }
    response
}

async fn success() -> Html<&'static str> {
    Html(SUCCESS_PAGE)
}

const SUCCESS_PAGE: &str = "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\
<meta charset=\"utf-8\"><title>Signed in</title></head>\n<body style=\"font-family: \
system-ui, sans-serif; text-align: center; margin-top: 4em\">\n<h1>Signed in</h1>\n\
<p>open-ferry has your credentials. You can close this window.</p>\n</body>\n</html>\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_the_verifiers_hash() {
        let pkce = Pkce::generate();
        assert_eq!(pkce.verifier.len(), 128);
        let hash = Sha256::digest(pkce.verifier.as_bytes());
        assert_eq!(pkce.challenge, URL_SAFE_NO_PAD.encode(hash));
        assert_ne!(Pkce::generate().verifier, pkce.verifier);
        assert_eq!(generate_state().len(), 43);
    }

    async fn get(port: u16, path_and_query: &str) -> (u16, String) {
        let response = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
            .get(format!("http://127.0.0.1:{port}{path_and_query}"))
            .send()
            .await
            .unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    #[tokio::test]
    async fn callback_gives_the_code() {
        let mut server = CallbackServer::start(0, "/auth/callback", "s1")
            .await
            .unwrap();
        let port = server.port();
        assert_eq!(get(port, "/auth/callback?code=c1&state=wrong").await.0, 400);
        assert_eq!(get(port, "/auth/callback?code=c1").await.0, 400);
        assert_eq!(get(port, "/auth/callback?code=c1&state=s1").await.0, 302);
        let result = server.wait(Duration::from_secs(5)).await.unwrap();
        assert_eq!(result, CallbackResult::Code("c1".into()));
        let (status, page) = get(port, "/success?platform_url=<script>").await;
        assert_eq!(status, 200);
        assert!(!page.contains("<script>"));
    }

    #[tokio::test]
    async fn callback_gives_the_providers_error() {
        let mut server = CallbackServer::start(0, "/callback", "s1").await.unwrap();
        assert_eq!(
            get(server.port(), "/callback?error=access_denied&state=s1")
                .await
                .0,
            400
        );
        let result = server.wait(Duration::from_secs(5)).await.unwrap();
        assert_eq!(result, CallbackResult::Error("access_denied".into()));
    }

    // An error report without the login's state is turned away like any other
    // callback, and the login goes on waiting for the real one.
    #[tokio::test]
    async fn error_needs_the_state_too() {
        let mut server = CallbackServer::start(0, "/callback", "s1").await.unwrap();
        let port = server.port();
        for query in [
            "error=access_denied&state=wrong",
            "error=access_denied",
            "error=access_denied&state=",
            "state=wrong",
        ] {
            let (status, body) = get(port, &format!("/callback?{query}")).await;
            assert_eq!(status, 400, "{query}");
            assert_eq!(body, "State parameter doesn't match", "{query}");
        }
        let error = server.wait(Duration::from_millis(50)).await.unwrap_err();
        assert!(matches!(error, CallbackError::Timeout), "{error}");
        assert_eq!(get(port, "/callback?code=c1&state=s1").await.0, 302);
        let result = server.wait(Duration::from_secs(5)).await.unwrap();
        assert_eq!(result, CallbackResult::Code("c1".into()));
    }

    /// Short limits, so the tests don't wait 10 seconds.
    const SHORT: Limits = Limits {
        connection: Duration::from_millis(300),
        shutdown: Duration::from_millis(200),
    };

    /// Opens a connection and sends a request that never finishes its
    /// headers.
    async fn half_open(port: u16) -> TcpStream {
        use tokio::io::AsyncWriteExt;
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        stream
            .write_all(b"GET /callback HTTP/1.1\r\nHost: localhost\r\n")
            .await
            .unwrap();
        stream
    }

    /// Waits up to `limit` for the server to close `stream`, and says how long
    /// it took, or `None` if it stayed open.
    async fn closed_within(stream: &mut TcpStream, limit: Duration) -> Option<Duration> {
        use tokio::io::AsyncReadExt;
        let started = Instant::now();
        let mut buf = [0; 256];
        loop {
            match tokio::time::timeout(limit, stream.read(&mut buf)).await {
                Err(_) => return None,
                Ok(Ok(0) | Err(_)) => return Some(started.elapsed()),
                // A 408 or similar before closing is fine.
                Ok(Ok(_)) => {}
            }
        }
    }

    #[test]
    fn limits_are_upstreams() {
        assert_eq!(Limits::DEFAULT.connection, Duration::from_secs(10));
        assert_eq!(Limits::DEFAULT.shutdown, Duration::from_secs(5));
    }

    // A request whose headers never finish is cut off at the deadline, and
    // the server still answers others.
    #[tokio::test]
    async fn unfinished_request_times_out() {
        let mut server = CallbackServer::start_with(0, "/callback", "s1", SHORT)
            .await
            .unwrap();
        let port = server.port();
        let mut stream = half_open(port).await;
        let took = closed_within(&mut stream, Duration::from_secs(5))
            .await
            .expect("the unfinished request outlived its deadline");
        assert!(took >= Duration::from_millis(200), "{took:?}");
        assert_eq!(get(port, "/callback?code=c1&state=s1").await.0, 302);
        let result = server.wait(Duration::from_secs(5)).await.unwrap();
        assert_eq!(result, CallbackResult::Code("c1".into()));
    }

    // Dropping the server closes a connection stuck in its headers after the
    // shutdown grace, well before the connection's own deadline.
    #[tokio::test]
    async fn dropping_the_server_closes_unfinished_requests() {
        let limits = Limits {
            connection: Duration::from_secs(60),
            shutdown: Duration::from_millis(100),
        };
        let server = CallbackServer::start_with(0, "/callback", "s1", limits)
            .await
            .unwrap();
        let port = server.port();
        let mut stream = half_open(port).await;
        // Let the server take the connection and start reading it.
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(server);
        let took = closed_within(&mut stream, Duration::from_secs(5))
            .await
            .expect("the unfinished request outlived the server");
        assert!(took < Duration::from_secs(2), "{took:?}");
    }

    // A response already on its way when the server is dropped still
    // arrives, as a graceful shutdown lets it.
    #[tokio::test]
    async fn dropping_the_server_lets_a_redirect_finish() {
        let mut server = CallbackServer::start(0, "/callback", "s1").await.unwrap();
        let port = server.port();
        let redirect = tokio::spawn(async move { get(port, "/callback?code=c1&state=s1").await });
        let result = server.wait(Duration::from_secs(5)).await.unwrap();
        drop(server);
        assert_eq!(result, CallbackResult::Code("c1".into()));
        assert_eq!(redirect.await.unwrap().0, 302);
    }

    #[tokio::test]
    async fn waiting_times_out() {
        let mut server = CallbackServer::start(0, "/callback", "s1").await.unwrap();
        let error = server.wait(Duration::from_millis(10)).await.unwrap_err();
        assert!(matches!(error, CallbackError::Timeout));
    }
}
