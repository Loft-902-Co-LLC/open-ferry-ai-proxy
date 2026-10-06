// Ported from CLIProxyAPI internal/auth/codex/pkce.go and oauth_server.go,
// which internal/auth/claude repeats (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What every OAuth login needs: PKCE codes, a `state` value, and a local
//! server for the provider's redirect.
//!
//! The callback server sends the browser on from the redirect to its
//! success page, which waits for the login's outcome and reports it (see
//! [`page`]). The login keeps the server up through the code exchange and
//! the save, then [finishes](CallbackServer::finish) it with the outcome.
//!
//! Deviations from upstream:
//! - The callback server listens on 127.0.0.1 only; upstream listens on
//!   every interface.
//! - The pages are this project's own (see [`page`]) and take nothing from
//!   the query. Upstream's success page copies a `platform_url` parameter
//!   into its HTML unescaped.
//! - The success page reports how the login ended. It waits up to 8
//!   seconds for the outcome, which fits in the connection deadline, and
//!   then says the sign-in is still finishing. Upstream's page always says
//!   the login succeeded, before the code is even exchanged.
//! - A failed callback is answered with a page saying what went wrong, and
//!   400, as upstream answers. The provider's error is named only when RFC
//!   6749 defines it; upstream's `OAuth error: <error>` text quotes any.
//! - A callback whose `state` isn't the login's, an error report included,
//!   is turned away with 400 and doesn't end the wait. Upstream hands a code
//!   on and checks its state later, and takes an error report as the
//!   login's without checking it.
//! - Upstream's 10-second read and write deadlines are one deadline here: a
//!   connection is closed once it has gone 10 seconds since connecting, or
//!   since its last response, without sending a whole request, or without
//!   taking the response it is owed.
//! - Upstream's login stops the server once the code is exchanged, before
//!   the credential is saved. Here the server lasts until the login is
//!   finished, saved or not, and then until the success page has answered
//!   or 5 seconds have passed without the browser asking for it.
//! - A stopped server gives its open connections 5 seconds (upstream's
//!   shutdown timeout) to finish and then closes them; upstream's shutdown
//!   gives up waiting and leaves them to their deadlines.

pub mod page;

use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::StatusCode;
use open_ferry_core::auth::Auth;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, Sleep};

use page::{Failure, Origin, Outcome};

/// The errors RFC 6749 (section 4.1.2.1) lets an authorization server send
/// to the redirect URI: the only callback errors named in logs and pages,
/// since anyone may send a callback anything.
pub const CALLBACK_ERRORS: [&str; 7] = [
    "invalid_request",
    "unauthorized_client",
    "access_denied",
    "unsupported_response_type",
    "invalid_scope",
    "server_error",
    "temporarily_unavailable",
];

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

/// What a browser login gives: its credential, still to be saved, and its
/// callback server. The caller [finishes](CallbackServer::finish) the server
/// with how the save went, so that the browser's page can say.
#[derive(Debug)]
pub struct BrowserLogin {
    /// The credential.
    pub auth: Auth,
    /// The callback server, still up.
    pub server: CallbackServer,
}

/// A local server for a provider's OAuth redirect (upstream's
/// `OAuthServer`).
///
/// [`finish`](Self::finish) gives its success page the login's outcome and
/// stops it. Dropped instead, it stops at once, and a success page still
/// waiting says the sign-in stopped.
pub struct CallbackServer {
    port: u16,
    result: oneshot::Receiver<CallbackResult>,
    shutdown: watch::Sender<bool>,
    /// The login's outcome, for the success page: `None` until it is known.
    outcome: watch::Sender<Option<Outcome>>,
    /// Whether the callback sent the browser to the success page.
    redirected: Arc<AtomicBool>,
    /// Turns true once the success page has answered after a redirect.
    shown: watch::Receiver<bool>,
    /// The server's task, until it is awaited.
    task: Option<JoinHandle<()>>,
    limits: Limits,
}

impl std::fmt::Debug for CallbackServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallbackServer")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct CallbackState {
    state: Arc<str>,
    /// The provider's name on the pages, such as `Codex`.
    provider: Arc<str>,
    result: Arc<Mutex<Option<oneshot::Sender<CallbackResult>>>>,
    outcome: watch::Receiver<Option<Outcome>>,
    redirected: Arc<AtomicBool>,
    shown: Arc<watch::Sender<bool>>,
    /// How long the success page waits for the outcome.
    page_wait: Duration,
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

/// How long the success page waits for the login's outcome. The page must
/// answer within the connection's deadline, 10 seconds from when the
/// browser connected or was last answered, so this leaves 2 seconds for
/// sending it.
const PAGE_WAIT: Duration = Duration::from_secs(8);

/// How long a finished server waits for the browser to ask for the success
/// page, after the callback sent it there.
const PAGE_GRACE: Duration = Duration::from_secs(5);

/// The callback server's time limits.
#[derive(Clone, Copy, Debug)]
struct Limits {
    connection: Duration,
    shutdown: Duration,
    /// How long the success page waits for the outcome.
    page: Duration,
    /// How long a finished server waits for the success page to be asked
    /// for and answered.
    grace: Duration,
}

impl Limits {
    const DEFAULT: Self = Self {
        connection: CONNECTION_TIMEOUT,
        shutdown: SHUTDOWN_GRACE,
        page: PAGE_WAIT,
        grace: PAGE_GRACE,
    };
}

impl CallbackServer {
    /// Listens on 127.0.0.1:`port` (0 for any free port) for a redirect to
    /// `path` that carries `state`, for a login to `provider`, the name its
    /// pages give, such as `Codex`.
    pub async fn start(port: u16, path: &str, state: &str, provider: &str) -> io::Result<Self> {
        Self::start_with(port, path, state, provider, Limits::DEFAULT).await
    }

    async fn start_with(
        port: u16,
        path: &str,
        state: &str,
        provider: &str,
        limits: Limits,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await?;
        let port = listener.local_addr()?.port();
        let (sender, result) = oneshot::channel();
        let (outcome, outcome_seen) = watch::channel(None);
        let (shown_sender, shown) = watch::channel(false);
        let redirected = Arc::new(AtomicBool::new(false));
        let app = Router::new()
            .route(path, get(callback))
            .route("/success", get(success))
            .with_state(CallbackState {
                state: state.into(),
                provider: provider.into(),
                result: Arc::new(Mutex::new(Some(sender))),
                outcome: outcome_seen,
                redirected: Arc::clone(&redirected),
                shown: Arc::new(shown_sender),
                page_wait: limits.page,
            });
        let (shutdown, stopped) = watch::channel(false);
        let listener = CallbackListener {
            inner: listener,
            stopped: stopped.clone(),
            limits,
        };
        let task = tokio::spawn(async move {
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
            outcome,
            redirected,
            shown,
            task: Some(task),
            limits,
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

    /// Gives the success page the login's `outcome`, and stops the server.
    ///
    /// If the callback sent the browser to the success page, the server
    /// first waits until the page has answered, or up to 5 seconds for the
    /// browser to ask for it. Then it waits for the open connections to
    /// finish, at most 5 seconds more, so that the page's answer leaves
    /// before this returns.
    pub async fn finish(mut self, outcome: Outcome) {
        self.outcome.send_replace(Some(outcome));
        if self.redirected.load(Ordering::SeqCst) {
            let shown = self.shown.wait_for(|shown| *shown);
            let _ = tokio::time::timeout(self.limits.grace, shown).await;
        }
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
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
    let failed = |failure| {
        page::response(
            &server.provider,
            Origin::Terminal,
            &Outcome::Failed(failure),
        )
    };
    // Nothing without the login's state counts, not even an error report:
    // anyone who can reach the port could otherwise end the login.
    if nonempty(query.state).as_deref() != Some(&*server.state) {
        return failed(Failure::new(
            "This page isn't from your sign-in",
            "It didn't come from the sign-in the terminal is waiting for, so open-ferry \
             ignored it.",
        ));
    }
    let (result, response) = if let Some(error) = nonempty(query.error) {
        let response = failed(Failure::provider_error(&server.provider, &error));
        (CallbackResult::Error(error), response)
    } else if let Some(code) = nonempty(query.code) {
        // Before the code goes, so that the login, once it has it, knows
        // the browser is on its way to the success page.
        server.redirected.store(true, Ordering::SeqCst);
        let response = (
            StatusCode::FOUND,
            [
                (http::header::LOCATION, "/success"),
                (http::header::CACHE_CONTROL, "no-store"),
                (http::header::REFERRER_POLICY, "no-referrer"),
            ],
        )
            .into_response();
        (CallbackResult::Code(code), response)
    } else {
        let response = failed(Failure::new(
            "No sign-in code came back",
            format!(
                "{} sent you back without a sign-in code, so the sign-in has stopped.",
                server.provider
            ),
        ));
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

/// The success page: how the login ended, once it has, or that it is still
/// finishing if it hasn't within the page's wait.
async fn success(State(server): State<CallbackState>) -> Response {
    if !server.redirected.load(Ordering::SeqCst) {
        let failure = Failure::new(
            "No sign-in to report yet",
            "No sign-in code has reached open-ferry yet. To sign in, open the link the \
             terminal shows.",
        );
        return page::response(
            &server.provider,
            Origin::Terminal,
            &Outcome::Failed(failure),
        );
    }
    let mut outcome = server.outcome.clone();
    let outcome =
        match tokio::time::timeout(server.page_wait, outcome.wait_for(Option::is_some)).await {
            Ok(Ok(outcome)) => outcome.clone().unwrap_or(Outcome::Finishing),
            // The login dropped the server without finishing it.
            Ok(Err(_)) => Outcome::Failed(Failure::stopped()),
            Err(_) => Outcome::Finishing,
        };
    let response = page::response(&server.provider, Origin::Terminal, &outcome);
    server.shown.send_replace(true);
    response
}

#[cfg(test)]
pub(crate) mod tests {
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

    /// An answer: its status, headers and body.
    pub(crate) struct Answer {
        pub(crate) status: u16,
        pub(crate) headers: reqwest::header::HeaderMap,
        pub(crate) body: String,
    }

    impl Answer {
        pub(crate) fn header(&self, name: &str) -> Option<&str> {
            self.headers.get(name).and_then(|value| value.to_str().ok())
        }
    }

    /// `GET path_and_query` on 127.0.0.1:`port`, without following
    /// redirects.
    pub(crate) async fn fetch(port: u16, path_and_query: &str) -> Answer {
        let response = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
            .get(format!("http://127.0.0.1:{port}{path_and_query}"))
            .send()
            .await
            .unwrap();
        Answer {
            status: response.status().as_u16(),
            headers: response.headers().clone(),
            body: response.text().await.unwrap(),
        }
    }

    async fn get(port: u16, path_and_query: &str) -> (u16, String) {
        let answer = fetch(port, path_and_query).await;
        (answer.status, answer.body)
    }

    /// Whether 127.0.0.1:`port` takes connections.
    async fn listening(port: u16) -> bool {
        let connect = TcpStream::connect((Ipv4Addr::LOCALHOST, port));
        matches!(
            tokio::time::timeout(Duration::from_secs(3), connect).await,
            Ok(Ok(_))
        )
    }

    /// Short limits, so the tests don't wait seconds.
    const SHORT: Limits = Limits {
        connection: Duration::from_millis(300),
        shutdown: Duration::from_millis(200),
        page: Duration::from_millis(200),
        grace: Duration::from_millis(300),
    };

    /// A Codex login's server with `limits`, sent the code `c1` and done
    /// waiting for it.
    async fn redirected(limits: Limits) -> CallbackServer {
        let mut server = CallbackServer::start_with(0, "/callback", "s1", "Codex", limits)
            .await
            .unwrap();
        let answer = fetch(server.port(), "/callback?code=c1&state=s1").await;
        assert_eq!(answer.status, 302);
        assert_eq!(answer.header("location"), Some("/success"));
        assert_eq!(answer.header("cache-control"), Some("no-store"));
        assert_eq!(answer.header("referrer-policy"), Some("no-referrer"));
        let result = server.wait(Duration::from_secs(5)).await.unwrap();
        assert_eq!(result, CallbackResult::Code("c1".into()));
        server
    }

    /// Asks for the success page of the server on `port` in the background,
    /// and checks it is waiting for the outcome.
    async fn success_page(port: u16) -> JoinHandle<Answer> {
        let page = tokio::spawn(async move { fetch(port, "/success").await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!page.is_finished(), "the page didn't wait for the outcome");
        page
    }

    /// Checks `answer` is the page with `status`, headed `title`.
    pub(crate) fn assert_page(answer: &Answer, status: u16, title: &str) {
        assert_eq!(answer.status, status, "{}", answer.body);
        assert!(
            answer.body.contains(&format!("<h1>{title}</h1>")),
            "{title}: {}",
            answer.body
        );
        assert_eq!(
            answer.header("content-type"),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(answer.header("cache-control"), Some("no-store"));
        assert_eq!(answer.header("referrer-policy"), Some("no-referrer"));
        let policy = answer.header("content-security-policy").unwrap();
        assert!(
            policy.starts_with("default-src 'none'; style-src 'sha256-"),
            "{policy}"
        );
        assert!(!answer.body.contains("<script"), "{}", answer.body);
        assert!(!answer.body.contains("window.close"), "{}", answer.body);
    }

    // Not upstream's (`TestOAuthServer` isn't ported: it listens on every
    // interface): the callback hands over the code and sends the browser on,
    // and the success page takes nothing from its query.
    #[tokio::test]
    async fn callback_gives_the_code() {
        let mut server = CallbackServer::start(0, "/auth/callback", "s1", "Codex")
            .await
            .unwrap();
        let port = server.port();
        assert_eq!(get(port, "/auth/callback?code=c1&state=wrong").await.0, 400);
        assert_eq!(get(port, "/auth/callback?code=c1").await.0, 400);
        assert_eq!(get(port, "/auth/callback?code=c1&state=s1").await.0, 302);
        let result = server.wait(Duration::from_secs(5)).await.unwrap();
        assert_eq!(result, CallbackResult::Code("c1".into()));
        let page = tokio::spawn(async move { fetch(port, "/success?platform_url=<script>").await });
        server.finish(Outcome::SignedIn).await;
        let page = page.await.unwrap();
        assert_page(&page, 200, "Signed in to Codex");
        assert!(!page.body.contains("platform_url"), "{}", page.body);
    }

    // Not upstream's: the provider's error, and a callback with neither code
    // nor error, end the wait with a page saying so; an error RFC 6749
    // doesn't define isn't repeated.
    #[tokio::test]
    async fn callback_gives_the_providers_error() {
        let mut server = CallbackServer::start(0, "/callback", "s1", "Claude")
            .await
            .unwrap();
        let answer = fetch(server.port(), "/callback?error=access_denied&state=s1").await;
        assert_page(&answer, 400, "Claude didn&#39;t sign you in");
        assert!(
            answer.body.contains("<code>access_denied</code>"),
            "{}",
            answer.body
        );
        assert!(
            answer.body.contains("go back to the terminal"),
            "{}",
            answer.body
        );
        let result = server.wait(Duration::from_secs(5)).await.unwrap();
        assert_eq!(result, CallbackResult::Error("access_denied".into()));

        let mut server = CallbackServer::start(0, "/callback", "s1", "Claude")
            .await
            .unwrap();
        let answer = fetch(server.port(), "/callback?error=%3Cb%3Ecall%20us&state=s1").await;
        assert_page(&answer, 400, "Claude didn&#39;t sign you in");
        assert!(!answer.body.contains("call us"), "{}", answer.body);
        let result = server.wait(Duration::from_secs(5)).await.unwrap();
        assert_eq!(result, CallbackResult::Error("<b>call us".into()));

        let mut server = CallbackServer::start(0, "/callback", "s1", "Claude")
            .await
            .unwrap();
        let answer = fetch(server.port(), "/callback?state=s1&code=").await;
        assert_page(&answer, 400, "No sign-in code came back");
        let result = server.wait(Duration::from_secs(5)).await.unwrap();
        assert_eq!(result, CallbackResult::Error("no_code".into()));
    }

    // Not upstream's: an error report without the login's state is turned
    // away like any other callback, and the login goes on waiting for the
    // real one.
    #[tokio::test]
    async fn error_needs_the_state_too() {
        let mut server = CallbackServer::start(0, "/callback", "s1", "Codex")
            .await
            .unwrap();
        let port = server.port();
        for query in [
            "error=access_denied&state=wrong",
            "error=access_denied",
            "error=access_denied&state=",
            "state=wrong",
        ] {
            let answer = fetch(port, &format!("/callback?{query}")).await;
            assert_page(&answer, 400, "This page isn&#39;t from your sign-in");
            assert!(!answer.body.contains("access_denied"), "{query}");
        }
        let error = server.wait(Duration::from_millis(50)).await.unwrap_err();
        assert!(matches!(error, CallbackError::Timeout), "{error}");
        assert_eq!(get(port, "/callback?code=c1&state=s1").await.0, 302);
        let result = server.wait(Duration::from_secs(5)).await.unwrap();
        assert_eq!(result, CallbackResult::Code("c1".into()));
    }

    // Not upstream's: the success page, asked for after the login has the
    // code, waits for the login to finish and says how it ended.
    #[tokio::test]
    async fn the_success_page_reports_the_outcome() {
        let server = redirected(Limits::DEFAULT).await;
        let page = success_page(server.port()).await;
        server.finish(Outcome::SignedIn).await;
        let answer = page.await.unwrap();
        assert_page(&answer, 200, "Signed in to Codex");
        assert!(
            answer
                .body
                .contains("You can close this tab and go back to the terminal."),
            "{}",
            answer.body
        );

        let server = redirected(Limits::DEFAULT).await;
        let page = success_page(server.port()).await;
        let failure = Failure::unfinished("Codex", "token exchange failed with status 400");
        server.finish(Outcome::Failed(failure)).await;
        let answer = page.await.unwrap();
        assert_page(&answer, 400, "The sign-in didn&#39;t finish");
        assert!(
            answer
                .body
                .contains("<code>token exchange failed with status 400</code>"),
            "{}",
            answer.body
        );
    }

    // Not upstream's: a login slower than the page's wait has it say the
    // sign-in is still finishing; one that drops its server has it say the
    // sign-in stopped; and the page asked for before any code says there is
    // nothing to report.
    #[tokio::test]
    async fn the_success_page_says_when_it_cant_tell() {
        let limits = Limits {
            page: Duration::from_millis(200),
            ..Limits::DEFAULT
        };
        let server = redirected(limits).await;
        let answer = fetch(server.port(), "/success").await;
        assert_page(&answer, 200, "The sign-in is still finishing");
        assert!(
            answer
                .body
                .contains("go back to the terminal, which shows the result."),
            "{}",
            answer.body
        );
        // The page has answered, so the server stops without its grace.
        let started = Instant::now();
        server.finish(Outcome::SignedIn).await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );

        let server = redirected(Limits::DEFAULT).await;
        let page = success_page(server.port()).await;
        drop(server);
        assert_page(&page.await.unwrap(), 400, "The sign-in stopped");

        let server = CallbackServer::start(0, "/callback", "s1", "Codex")
            .await
            .unwrap();
        let answer = fetch(server.port(), "/success").await;
        assert_page(&answer, 400, "No sign-in to report yet");
    }

    // Not upstream's: a finished server stops once the success page has
    // answered, or after the grace when the browser doesn't ask for it, or
    // at once when the callback didn't send the browser there.
    #[tokio::test]
    async fn finishing_stops_the_server() {
        let server = redirected(Limits::DEFAULT).await;
        let port = server.port();
        let page = tokio::spawn(async move { fetch(port, "/success").await });
        let started = Instant::now();
        server.finish(Outcome::SignedIn).await;
        let took = started.elapsed();
        assert!(took < Duration::from_secs(2), "{took:?}");
        assert_page(&page.await.unwrap(), 200, "Signed in to Codex");
        assert!(!listening(port).await, "the server outlived the page");

        let server = redirected(SHORT).await;
        let port = server.port();
        let started = Instant::now();
        server.finish(Outcome::SignedIn).await;
        let took = started.elapsed();
        assert!(took >= SHORT.grace, "{took:?}");
        assert!(took < Duration::from_secs(2), "{took:?}");
        assert!(!listening(port).await, "the server outlived the grace");

        let mut server = CallbackServer::start(0, "/callback", "s1", "Codex")
            .await
            .unwrap();
        let port = server.port();
        let answer = fetch(port, "/callback?error=access_denied&state=s1").await;
        assert_eq!(answer.status, 400);
        server.wait(Duration::from_secs(5)).await.unwrap();
        let started = Instant::now();
        server.finish(Outcome::Failed(Failure::stopped())).await;
        let took = started.elapsed();
        assert!(took < Duration::from_secs(2), "{took:?}");
        assert!(!listening(port).await, "the server outlived the login");
    }

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

    // Not upstream's: the success page answers well inside the connection's
    // deadline, and a finished server waits a few seconds for the browser.
    #[test]
    fn page_limits_fit_the_connection_deadline() {
        assert_eq!(Limits::DEFAULT.page, Duration::from_secs(8));
        assert!(Limits::DEFAULT.page + Duration::from_secs(2) <= Limits::DEFAULT.connection);
        assert_eq!(Limits::DEFAULT.grace, Duration::from_secs(5));
    }

    // A request whose headers never finish is cut off at the deadline, and
    // the server still answers others.
    #[tokio::test]
    async fn unfinished_request_times_out() {
        let mut server = CallbackServer::start_with(0, "/callback", "s1", "Codex", SHORT)
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
            ..Limits::DEFAULT
        };
        let server = CallbackServer::start_with(0, "/callback", "s1", "Codex", limits)
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
        let mut server = CallbackServer::start(0, "/callback", "s1", "Codex")
            .await
            .unwrap();
        let port = server.port();
        let redirect = tokio::spawn(async move { get(port, "/callback?code=c1&state=s1").await });
        let result = server.wait(Duration::from_secs(5)).await.unwrap();
        drop(server);
        assert_eq!(result, CallbackResult::Code("c1".into()));
        assert_eq!(redirect.await.unwrap().0, 302);
    }

    #[tokio::test]
    async fn waiting_times_out() {
        let mut server = CallbackServer::start(0, "/callback", "s1", "Codex")
            .await
            .unwrap();
        let error = server.wait(Duration::from_millis(10)).await.unwrap_err();
        assert!(matches!(error, CallbackError::Timeout));
    }
}
