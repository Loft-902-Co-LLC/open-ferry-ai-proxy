//! The dashboard's tests through its router: serving the app, the API's
//! access rules and errors, and each route. Every request comes from
//! 127.0.0.1 unless a test says otherwise, with the key [`KEY`] when made
//! with [`keyed`]. The log directory, and the ledger in it, are a temporary
//! directory of each test's own.
//!
//! The `/management.html` tests port what upstream tests of its control
//! panel route; the rest are open-ferry's own.

mod api;
mod client_setup;
mod request_logs;
mod serve;
mod usage;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use chrono::{DateTime, Utc};
use http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt as _;
use open_ferry_core::config::Config;
use open_ferry_core::manager::{Manager, Settings};
use open_ferry_core::observe::Observability;
use open_ferry_core::observe::usage::{
    ClientKey, InputBreakdown, OutputBreakdown, TokenBreakdown, Usage, UsageEvent,
};
use open_ferry_core::registry::ModelRegistry;
use open_ferry_management::ManagementState;
use serde_json::Value;
use tower::ServiceExt as _;

use crate::assets::Assets;
use crate::{DashboardState, Ledger, router_from};

/// The management key the tests set.
pub(crate) const KEY: &str = "test-secret";

/// Where requests come from unless a test says otherwise.
pub(crate) const LOCAL: &str = "127.0.0.1:50000";

/// A client on another host.
pub(crate) const REMOTE: &str = "203.0.113.7:50000";

/// The app the tests serve.
const APP: &[(&str, &[u8])] = &[
    ("index.html", b"<!doctype html><title>app</title>"),
    ("assets/index-3f2a9c.js", b"console.log(1)"),
    ("assets/index-77b1e0.css", b"body{}"),
    (
        "favicon.svg",
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
    ),
    ("third-party-licenses.txt", b"MIT"),
];

/// `time`, an RFC 3339 time, in milliseconds since the epoch.
pub(crate) fn ms(time: &str) -> i64 {
    DateTime::parse_from_rfc3339(time)
        .unwrap()
        .timestamp_millis()
}

/// A call that succeeded, started at `time` (RFC 3339), to `model` of
/// `provider`, for `POST /v1/chat/completions`, taking 100 ms, without a
/// credential, client key, stream or tokens; its request ID is `req-` and
/// `time`.
pub(crate) fn event(time: &str, provider: &str, model: &str) -> UsageEvent {
    UsageEvent {
        requested_at: DateTime::<Utc>::from_timestamp_millis(ms(time)).unwrap(),
        request_id: format!("req-{time}"),
        endpoint: "POST /v1/chat/completions".to_owned(),
        provider: provider.to_owned(),
        model: model.to_owned(),
        alias: model.to_owned(),
        credential: None,
        client_key: ClientKey::default(),
        stream: false,
        failed: false,
        status: 200,
        latency: Duration::from_millis(100),
        ttft: None,
        tokens: TokenBreakdown::default(),
        total_tokens: 0,
    }
}

/// A breakdown of `input` tokens, `cache_read` and `cache_write` of them
/// cached, and `output`, `reasoning` of them reasoning.
pub(crate) fn tokens(
    input: i64,
    cache_read: i64,
    cache_write: i64,
    output: i64,
    reasoning: i64,
) -> TokenBreakdown {
    TokenBreakdown {
        total_tokens: input + output,
        input: InputBreakdown {
            total_tokens: input,
            uncached_tokens: input - cache_read - cache_write,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
        },
        output: OutputBreakdown {
            total_tokens: output,
            non_reasoning_tokens: output - reasoning,
            reasoning_tokens: reasoning,
        },
        ..TokenBreakdown::default()
    }
}

/// The default config, with management key [`KEY`] and the usage
/// statistics on.
pub(crate) fn keyed_config() -> Config {
    let mut config = Config::default();
    config.remote_management.secret_key = KEY.into();
    config.usage_statistics_enabled = true;
    config
}

/// The dashboard's router over a state of its own.
pub(crate) struct Dash {
    router: Router,
    state: DashboardState,
    registry: Arc<ModelRegistry>,
    logs: tempfile::TempDir,
}

impl Dash {
    /// With [`keyed_config`], the test app and an open ledger.
    pub(crate) fn new() -> Self {
        Self::with_config(keyed_config())
    }

    /// With `config`, the test app and an open ledger.
    pub(crate) fn with_config(config: Config) -> Self {
        Self::build(config, Assets::fixture(APP), true)
    }

    /// With `config`, the test app, an open ledger, and `password` as the
    /// local management password.
    pub(crate) fn with_local_password(config: Config, password: &str) -> Self {
        let mut dash = Self::with_config(config);
        dash.state.management = dash.state.management.clone().with_local_password(password);
        dash.router = router_from(dash.state.clone());
        dash
    }

    /// With `config`, `assets`, and an open ledger if `ledger`, else one
    /// that is unavailable.
    pub(crate) fn build(config: Config, assets: Assets, ledger: bool) -> Self {
        let logs = tempfile::tempdir().unwrap();
        let registry = Arc::new(ModelRegistry::new());
        let manager = Manager::new(Settings::default(), Arc::clone(&registry) as _, None);
        let usage = Usage::new(&config);
        let management =
            ManagementState::new(Arc::new(config), manager, Arc::clone(&registry), None)
                .with_observability(Observability {
                    log_dir: Some(logs.path().to_owned()),
                    ..Observability::default()
                });
        let ledger = if ledger {
            Ledger::idle_for_test(logs.path(), &usage)
        } else {
            Ledger::unavailable("the test has none")
        };
        let state = DashboardState {
            management,
            ledger,
            assets,
        };
        Self {
            router: router_from(state.clone()),
            state,
            registry,
            logs,
        }
    }

    /// The log directory.
    pub(crate) fn logs(&self) -> &Path {
        self.logs.path()
    }

    /// The model registry.
    pub(crate) fn registry(&self) -> &ModelRegistry {
        &self.registry
    }

    /// Writes `events` into the ledger.
    pub(crate) fn record(&self, events: &[UsageEvent]) {
        self.state.ledger.insert_for_test(events);
    }

    /// The answer to `request`.
    pub(crate) async fn send(&self, request: Request<Body>) -> Answer {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        let body = body.collect().await.unwrap().to_bytes();
        Answer {
            status: parts.status,
            headers: parts.headers,
            body: String::from_utf8_lossy(&body).into_owned(),
            bytes: body.to_vec(),
        }
    }

    /// `GET path`, with the key.
    pub(crate) async fn get(&self, path: &str) -> Answer {
        self.send(keyed(Method::GET, path, "")).await
    }

    /// `method path` with `body`, with the key.
    pub(crate) async fn call(&self, method: Method, path: &str, body: &str) -> Answer {
        self.send(keyed(method, path, body)).await
    }
}

/// A request from `peer`, without a key.
pub(crate) fn request(peer: &str, method: Method, path: &str, body: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let peer: SocketAddr = peer.parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(peer));
    request
}

/// A request from [`LOCAL`] with the key as a bearer token.
pub(crate) fn keyed(method: Method, path: &str, body: &str) -> Request<Body> {
    let mut request = request(LOCAL, method, path, body);
    let value = format!("Bearer {KEY}").parse().unwrap();
    request.headers_mut().insert(header::AUTHORIZATION, value);
    request
}

/// A response, read.
#[derive(Debug)]
pub(crate) struct Answer {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    /// The body as text, any bytes that aren't UTF-8 as U+FFFD.
    pub(crate) body: String,
    /// The body.
    pub(crate) bytes: Vec<u8>,
}

impl Answer {
    /// The body as JSON, after checking the status.
    pub(crate) fn json(&self, status: StatusCode) -> Value {
        assert_eq!(self.status, status, "{}", self.body);
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }

    /// Checks that this is the API's error `code` with `status`, and
    /// returns its message.
    pub(crate) fn error(&self, status: StatusCode, code: &str) -> String {
        let body = self.json(status);
        assert_eq!(body["error"], code, "{body}");
        assert_eq!(
            self.header("content-type"),
            Some("application/json; charset=utf-8")
        );
        assert_eq!(self.header("cache-control"), Some("no-store"));
        body["message"].as_str().unwrap().to_owned()
    }

    /// The header `name`.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|value| value.to_str().unwrap())
    }
}
