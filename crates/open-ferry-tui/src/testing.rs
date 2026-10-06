//! A management server for tests, on a loopback port: it answers the
//! routes a test gives it, `{"error":"not found"}` with a 404 otherwise, and
//! records every request.

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

/// A request the server took.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) uri: String,
    pub(crate) path: String,
    pub(crate) auth: String,
    pub(crate) body: String,
}

impl fmt::Display for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} auth={} body={}",
            self.method, self.uri, self.auth, self.body
        )
    }
}

#[derive(Default)]
struct Shared {
    routes: Mutex<HashMap<String, String>>,
    requests: Mutex<Vec<Request>>,
}

/// The server; it stops when dropped.
pub(crate) struct Server {
    addr: SocketAddr,
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    /// Starts a server answering each `"METHOD /path"` route with its body.
    pub(crate) async fn start(routes: &[(&str, &str)]) -> Self {
        let shared = Arc::new(Shared::default());
        for (route, body) in routes {
            shared
                .routes
                .lock()
                .unwrap()
                .insert((*route).to_owned(), (*body).to_owned());
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .fallback(handle)
            .with_state(Arc::clone(&shared));
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, shared, task }
    }

    /// The server's base URL.
    pub(crate) fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The server's port.
    pub(crate) fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Answers `route` with `body` from now on.
    pub(crate) fn set(&self, route: &str, body: &str) {
        self.shared
            .routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(route.to_owned(), body.to_owned());
    }

    /// The requests taken so far.
    pub(crate) fn requests(&self) -> Vec<Request> {
        self.shared
            .requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The requests taken so far, as text, and forgets them.
    pub(crate) fn take_requests(&self) -> Vec<String> {
        let mut requests = self
            .shared
            .requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        requests.drain(..).map(|r| r.to_string()).collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle(
    State(shared): State<Arc<Shared>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let path = uri.path().to_owned();
    let request = Request {
        method: method.to_string(),
        uri: uri
            .path_and_query()
            .map_or_else(|| path.clone(), ToString::to_string),
        path: path.clone(),
        auth,
        body: String::from_utf8_lossy(&body).into_owned(),
    };
    shared
        .requests
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(request);
    let route = format!("{method} {path}");
    let found = shared
        .routes
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&route)
        .cloned();
    let json = [(header::CONTENT_TYPE, "application/json")];
    match found {
        Some(body) => (json, body).into_response(),
        None => (StatusCode::NOT_FOUND, json, r#"{"error":"not found"}"#).into_response(),
    }
}
