// Ported from CLIProxyAPI internal/api/server_keepalive.go (enableKeepAlive,
// handleKeepAlive, signalKeepAlive, watchKeepAlive) and internal/cmd/run.go
// (StartService's keep-alive) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The keep-alive endpoint of a server started with a local management
//! password (`-password`), as by a program that runs it and watches it.
//! `GET /keep-alive` with the password, as `Authorization: Bearer
//! <password>`, a bare `Authorization` value or `X-Local-Password`,
//! answers `{"status":"ok"}`; any other value answers 401
//! `{"error":"invalid password"}`. When no call has come for 10 seconds,
//! the server shuts down.
//!
//! Deviations from upstream: none.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use open_ferry_translate::go::trim_space;
use subtle::ConstantTimeEq as _;
use tokio::sync::Notify;

/// How long the server waits for a call before it shuts down.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// The endpoint and its watch.
#[derive(Clone)]
pub struct KeepAlive {
    password: Arc<[u8]>,
    heartbeat: Arc<Notify>,
    timeout: Duration,
}

impl KeepAlive {
    /// The endpoint for `password`, which the server shuts down without
    /// for `timeout`.
    pub fn new(password: &str, timeout: Duration) -> Self {
        Self {
            password: Arc::from(password.as_bytes()),
            heartbeat: Arc::new(Notify::new()),
            timeout,
        }
    }

    /// The `GET /keep-alive` route.
    pub fn router(&self) -> Router {
        Router::new()
            .route(
                "/keep-alive",
                get(keep_alive).head(not_found).fallback(not_found),
            )
            .with_state(self.clone())
    }

    /// Resolves once no call has come for the timeout (`watchKeepAlive`).
    pub async fn idle(&self) {
        loop {
            tokio::select! {
                () = tokio::time::sleep(self.timeout) => break,
                () = self.heartbeat.notified() => {}
            }
        }
        let message = format!(
            "keep-alive endpoint idle for {}s, shutting down",
            self.timeout.as_secs()
        );
        // Once from the watch, and once from the service's callback.
        tracing::warn!("{message}");
        tracing::warn!("{message}");
    }
}

/// `handleKeepAlive`.
async fn keep_alive(State(keep_alive): State<KeepAlive>, headers: HeaderMap) -> Response {
    let header = |name| trim_space(headers.get(name).map_or(&b""[..], HeaderValue::as_bytes));
    let mut provided = header(header::AUTHORIZATION.as_str());
    if let Some(space) = provided.iter().position(|&b| b == b' ')
        && let (Some(scheme), Some(rest)) = (provided.get(..space), provided.get(space + 1..))
        && scheme.eq_ignore_ascii_case(b"bearer")
    {
        provided = rest;
    }
    if provided.is_empty() {
        provided = header("x-local-password");
    }
    if !bool::from(provided.ct_eq(&keep_alive.password)) {
        return json(StatusCode::UNAUTHORIZED, r#"{"error":"invalid password"}"#);
    }
    // `signalKeepAlive`: a call already waiting to be seen is enough.
    keep_alive.heartbeat.notify_one();
    json(StatusCode::OK, r#"{"status":"ok"}"#)
}

/// `body` as gin's JSON.
fn json(status: StatusCode, body: &'static str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body,
    )
        .into_response()
}

/// Gin's 404, for the methods the route doesn't serve.
async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/plain")],
        "404 page not found",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    /// Sends `method /keep-alive` with `headers` to `keep_alive`'s route on
    /// a 127.0.0.1 ephemeral port; the status and body of the answer.
    async fn call(keep_alive: &KeepAlive, method: &str, headers: &str) -> (u16, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = keep_alive.router();
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request = format!(
            "{method} /keep-alive HTTP/1.1\r\nHost: {addr}\r\n{headers}Connection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        server.abort();
        let response = String::from_utf8(response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        (status, body.to_owned())
    }

    // Not upstream's: the password is read from each header as upstream's
    // handler reads it.
    #[tokio::test]
    async fn calls_need_the_password() {
        let keep_alive = KeepAlive::new("pw", TIMEOUT);
        let ok = (200, r#"{"status":"ok"}"#.to_owned());
        let refused = (401, r#"{"error":"invalid password"}"#.to_owned());
        for (headers, answer) in [
            ("Authorization: Bearer pw\r\n", &ok),
            ("Authorization: bEaReR pw\r\n", &ok),
            ("Authorization:  pw \r\n", &ok),
            ("X-Local-Password:  pw\r\n", &ok),
            (
                "Authorization: Bearer\r\nX-Local-Password: pw\r\n",
                &refused,
            ),
            (
                "Authorization: Basic pw\r\nX-Local-Password: pw\r\n",
                &refused,
            ),
            ("Authorization: Bearer pw2\r\n", &refused),
            ("", &refused),
        ] {
            assert_eq!(
                &call(&keep_alive, "GET", headers).await,
                answer,
                "{headers}"
            );
        }
        let (status, body) = call(&keep_alive, "POST", "X-Local-Password: pw\r\n").await;
        assert_eq!((status, body.as_str()), (404, "404 page not found"));
    }

    // Not upstream's: the server is told to shut down once no call has come
    // for the timeout, and a call starts the wait again.
    #[tokio::test(start_paused = true)]
    async fn idles_without_calls() {
        let keep_alive = KeepAlive::new("pw", TIMEOUT);
        let start = tokio::time::Instant::now();
        keep_alive.idle().await;
        assert_eq!(start.elapsed(), TIMEOUT);

        let idle = tokio::spawn({
            let keep_alive = keep_alive.clone();
            async move { keep_alive.idle().await }
        });
        let start = tokio::time::Instant::now();
        tokio::time::sleep(Duration::from_secs(9)).await;
        keep_alive.heartbeat.notify_one();
        tokio::time::sleep(Duration::from_secs(9)).await;
        assert!(!idle.is_finished());
        idle.await.unwrap();
        assert_eq!(start.elapsed(), Duration::from_secs(19));
    }
}
