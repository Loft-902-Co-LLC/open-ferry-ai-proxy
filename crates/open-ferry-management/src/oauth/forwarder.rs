// Ported from CLIProxyAPI internal/api/handlers/management/
// auth_files_oauth_callback.go (callbackForwarder, startCallbackForwarder,
// stopCallbackForwarderInstance, stopForwarderInstance) (v8.0.15, MIT),
// and Go's net/http/server.go (Redirect, htmlEscape) (go1.26.4,
// BSD-3-Clause), as the forwarder uses them.
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/golang/go

//! The callback forwarders: for a login started from the web UI, a server
//! on the port of the provider's redirect URI that sends the browser on to
//! the main server's callback page, the query unchanged.
//!
//! The provider sends the browser to its fixed redirect URI, on port 54545
//! for Claude and 1455 for Codex, which only a server on the same machine
//! as the browser can take. Each port has one forwarder: a login started on
//! a port that already has one replaces it. A forwarder stops when its
//! login ends, unless another has replaced it.
//!
//! Every request, whatever its method and path, is answered `302 Found`
//! with `Cache-Control: no-store`, as Go's `http.Redirect` answers: a `GET`
//! with a small HTML page linking to the target.
//!
//! Deviations from upstream:
//! - A forwarder listens on 127.0.0.1 only; upstream listens on every
//!   interface.
//! - A connection is closed once it has gone 5 seconds, since it connected
//!   or since its last answer, without sending a whole request or without
//!   taking its answer. Upstream gives 5 seconds to send a request's
//!   headers and 5 to take the answer, and leaves an idle connection open.
//! - A stopped forwarder closes its listener at once and leaves its open
//!   connections to finish or reach their deadline; upstream's shutdown
//!   gives them up to 2 seconds to finish.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::response::{IntoResponse, Response};
use http::{HeaderValue, Method, StatusCode, header};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::{Instant, Sleep};

/// How long a connection has to send a request, and to take its answer
/// (upstream's `ReadHeaderTimeout` and `WriteTimeout`).
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a replaced forwarder is given to close its listener
/// (upstream's shutdown timeout).
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// The running forwarders, by the port asked for (upstream's
/// `callbackForwarders`).
type Running = Arc<Mutex<HashMap<u16, Arc<Forwarder>>>>;

/// The callback forwarders.
#[derive(Debug, Default)]
pub(crate) struct Forwarders {
    running: Running,
}

/// A running forwarder (upstream's `callbackForwarder`).
#[derive(Debug)]
struct Forwarder {
    /// Where it listens.
    addr: SocketAddr,
    /// The server, until it is stopped.
    task: Mutex<Option<JoinHandle<()>>>,
}

/// A forwarder a login started: it stops when this is dropped, unless
/// another forwarder has replaced it (upstream's
/// `stopCallbackForwarderInstance`, deferred).
#[derive(Debug)]
pub(crate) struct Started {
    running: Running,
    port: u16,
    forwarder: Arc<Forwarder>,
}

impl Forwarders {
    /// Stops the forwarder on 127.0.0.1:`port` if there is one, then starts
    /// one there that sends every request to `target` with its query
    /// (`startCallbackForwarder`). Port 0 takes any free port.
    pub(crate) async fn start(&self, port: u16, target: String) -> io::Result<Started> {
        let previous = lock(&self.running).remove(&port);
        if let Some(previous) = previous {
            previous.stop().await;
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
        let addr = listener.local_addr()?;
        let app = Router::new()
            .fallback(redirect)
            .with_state(Arc::<str>::from(target));
        let task = tokio::spawn(async move {
            let listener = TimedListener { inner: listener };
            if let Err(error) = axum::serve(listener, app).await {
                tracing::warn!("OAuth callback forwarder on {addr} stopped: {error}");
            }
        });
        let forwarder = Arc::new(Forwarder {
            addr,
            task: Mutex::new(Some(task)),
        });
        lock(&self.running).insert(port, Arc::clone(&forwarder));
        tracing::info!("OAuth callback forwarder listening on {addr}");
        Ok(Started {
            running: Arc::clone(&self.running),
            port,
            forwarder,
        })
    }

    /// Where the forwarder started for `port` listens, while it runs.
    #[cfg(test)]
    pub(crate) fn addr(&self, port: u16) -> Option<SocketAddr> {
        lock(&self.running)
            .get(&port)
            .map(|forwarder| forwarder.addr)
    }
}

impl Forwarder {
    /// Stops the server, and waits up to [`STOP_TIMEOUT`] for its listener
    /// to close (`stopForwarderInstance`).
    async fn stop(&self) {
        let task = lock(&self.task).take();
        if let Some(task) = task {
            task.abort();
            let _ = tokio::time::timeout(STOP_TIMEOUT, task).await;
            tracing::info!("OAuth callback forwarder on {} stopped", self.addr);
        }
    }

    /// Stops the server without waiting.
    fn abort(&self) {
        if let Some(task) = lock(&self.task).take() {
            task.abort();
            tracing::info!("OAuth callback forwarder on {} stopped", self.addr);
        }
    }
}

impl Drop for Started {
    fn drop(&mut self) {
        {
            let mut running = lock(&self.running);
            let current = running
                .get(&self.port)
                .is_some_and(|forwarder| Arc::ptr_eq(forwarder, &self.forwarder));
            if current {
                running.remove(&self.port);
            }
        }
        self.forwarder.abort();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Sends a request on to `target`, with the request's query after a `?`,
/// or after a `&` when `target` already has a query.
async fn redirect(
    State(target): State<Arc<str>>,
    method: Method,
    RawQuery(query): RawQuery,
) -> Response {
    let mut target = target.to_string();
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        target.push(if target.contains('?') { '&' } else { '?' });
        target.push_str(&query);
    }
    found(&method, &target)
}

/// What Go's `http.Redirect` answers with `302 Found` for the absolute URL
/// `target`, with `Cache-Control: no-store` set before it. Go escapes the
/// non-ASCII bytes of the `Location`; a request's query holds none here.
fn found(method: &Method, target: &str) -> Response {
    let Ok(location) = HeaderValue::from_str(target) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let body = if method == Method::GET {
        format!("<a href=\"{}\">Found</a>.\n\n", html_escape(target))
    } else {
        String::new()
    };
    let mut response = (StatusCode::FOUND, body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::LOCATION, location);
    if method == Method::GET || method == Method::HEAD {
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
    } else {
        headers.remove(header::CONTENT_TYPE);
    }
    response
}

/// `text` with `&`, `<`, `>`, `"` and `'` escaped, as Go's `htmlEscape`
/// escapes them.
fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A forwarder's listener: a TCP listener whose connections have
/// [`CONNECTION_TIMEOUT`] for each request and answer.
struct TimedListener {
    inner: TcpListener,
}

impl axum::serve::Listener for TimedListener {
    type Io = TimedConnection;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (stream, addr) = axum::serve::Listener::accept(&mut self.inner).await;
        let connection = TimedConnection {
            stream,
            deadline: Box::pin(tokio::time::sleep(CONNECTION_TIMEOUT)),
        };
        (connection, addr)
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// A connection that fails, and so is closed, once it has gone
/// [`CONNECTION_TIMEOUT`] since it connected or last wrote.
struct TimedConnection {
    stream: TcpStream,
    deadline: Pin<Box<Sleep>>,
}

impl TimedConnection {
    /// Fails once the deadline has passed. Polling the deadline wakes the
    /// connection when it comes, even while the client sends nothing.
    fn check(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if self.deadline.as_mut().poll(cx).is_ready() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "OAuth callback forwarder connection timed out",
            ));
        }
        Ok(())
    }
}

impl AsyncRead for TimedConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.check(cx)?;
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for TimedConnection {
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
            // An answer went out; the next request gets a fresh deadline.
            let next = Instant::now() + CONNECTION_TIMEOUT;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Not upstream's: Go's `http.Redirect` escapes these five characters
    /// in the page's link.
    #[test]
    fn html_escape_escapes_as_go() {
        assert_eq!(
            html_escape(r#"http://h/p?a=1&b=<x>"y'"#),
            "http://h/p?a=1&amp;b=&lt;x&gt;&#34;y&#39;"
        );
    }
}
