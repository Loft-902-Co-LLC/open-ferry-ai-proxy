//! Tests of the request log's capture layer: the ports of CLIProxyAPI
//! internal/api/middleware/request_logging_test.go, response_writer_test.go
//! and internal/logging/cpa_trace_test.go (v8.0.10, MIT), and the redaction
//! and failed-attempt tests of the port's own.
//!
//! Each test serves its routes behind the request context and the capture
//! layer alone, or the whole router, with a request logger writing to a
//! directory of its own.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::middleware;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use http_body_util::BodyExt as _;
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::Format;
use open_ferry_core::observe::request_log::RequestLogger;
use open_ferry_core::observe::{
    AttemptKind, AttemptRequest, Observability, Outcome, RequestContext,
};
use tower::ServiceExt as _;

use crate::config::ServerConfig;
use crate::request_context;
use crate::state::AppState;
use crate::testing::{FakeCatalog, FakeDispatcher, state};

mod api_errors;
mod cpa_trace;
mod redaction;
mod request_logging;
mod response_writer;

/// A directory of the test's own, removed when it is dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("ofp-request-log-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        for _ in 0..3 {
            if fs::remove_dir_all(&self.0).is_ok() || !self.0.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// A server whose request logger writes to a directory of its own.
struct Harness {
    dir: TempDir,
    logger: RequestLogger,
    state: AppState,
}

impl Harness {
    /// With `request-log` on or off, and the server's config.
    fn with(request_log: bool, server: ServerConfig) -> Self {
        Self::over(
            request_log,
            state(server, FakeCatalog::new(), &FakeDispatcher::new([])),
        )
    }

    /// With `request-log` on or off, over `state`.
    fn over(request_log: bool, state: AppState) -> Self {
        let dir = TempDir::new();
        let mut config = Config::default();
        config.request_log = request_log;
        config.error_logs_max_files = 10;
        let logger = RequestLogger::new(&config, dir.path(), Path::new(""));
        let state = state.with_observability(Observability {
            log_dir: Some(dir.path().to_path_buf()),
            request_log: logger.clone(),
            ..Observability::default()
        });
        Self { dir, logger, state }
    }

    fn new(request_log: bool) -> Self {
        Self::with(request_log, ServerConfig::default())
    }

    /// `routes` behind the request context and the capture layer.
    fn app(&self, routes: Router<AppState>) -> Router {
        routes
            .with_state(self.state.clone())
            .layer(middleware::from_fn_with_state(
                self.state.clone(),
                super::layer,
            ))
            .layer(middleware::from_fn_with_state(
                self.state.clone(),
                request_context::layer,
            ))
    }

    /// The logs written so far, by name, once the writer is done.
    fn logs(&self) -> Vec<(String, String)> {
        self.logger.flush();
        let mut logs: Vec<_> = fs::read_dir(self.dir.path())
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    String::from_utf8_lossy(&fs::read(entry.path()).unwrap()).into_owned(),
                )
            })
            .collect();
        logs.sort();
        logs
    }

    /// The one log written, or a panic.
    fn only_log(&self) -> (String, String) {
        let mut logs = self.logs();
        assert_eq!(logs.len(), 1, "{logs:#?}");
        logs.remove(0)
    }
}

/// Sends `request` to `app`, and reads the whole answer.
async fn send(app: &Router, request: Request) -> (StatusCode, HeaderMap, Bytes) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, body)
}

/// A `POST` to `path` with a JSON `body`.
fn post(path: &str, body: impl Into<Body>) -> Request {
    Request::post(path)
        .header("content-type", "application/json")
        .body(body.into())
        .unwrap()
}

/// An upstream attempt, as an executor tells the request's taps of it.
struct Upstream<'a> {
    kind: AttemptKind,
    url: &'a str,
    body: &'a str,
    secret: &'a str,
    status: u16,
    chunks: &'a [&'a str],
}

impl Default for Upstream<'_> {
    fn default() -> Self {
        Self {
            kind: AttemptKind::Execute,
            url: "https://api.example.com/v1/responses",
            body: "{\"model\":\"gpt-5\"}",
            secret: "",
            status: 200,
            chunks: &[],
        }
    }
}

impl Upstream<'_> {
    /// Tells the taps of the request of `context` of the attempt.
    fn run(&self, state: &AppState, context: &Arc<RequestContext>) {
        let Some(tap) = state.observability().request_log.tap(context) else {
            return;
        };
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        if !self.secret.is_empty() {
            headers.insert(
                "authorization",
                HeaderValue::from_str(&format!("Bearer {}", self.secret)).unwrap(),
            );
        }
        let body = Bytes::from(self.body.to_owned());
        let auth = Auth {
            id: "codex-a.json".to_owned(),
            provider: "codex".to_owned(),
            ..Auth::default()
        };
        let secrets: Vec<&str> = [self.secret]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();
        tap.attempt_request(&AttemptRequest {
            kind: self.kind,
            method: if self.kind == AttemptKind::Websocket {
                &Method::GET
            } else {
                &Method::POST
            },
            url: self.url,
            headers: &headers,
            body: &body,
            provider: "codex",
            model: "gpt-5",
            format: &Format::from("codex"),
            auth: &auth,
            secrets: &secrets,
        });
        if self.kind != AttemptKind::Websocket {
            let mut response_headers = HeaderMap::new();
            response_headers.insert("content-type", HeaderValue::from_static("application/json"));
            tap.response_head(self.status, &response_headers);
        }
        for chunk in self.chunks {
            tap.chunk(&Bytes::from(chunk.to_string()));
        }
        tap.finish(Outcome::Completed);
    }
}
